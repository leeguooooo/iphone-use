import Foundation
@preconcurrency import AVFoundation
@preconcurrency import Speech
import Combine

@MainActor
protocol AssistantVoiceAudio: AnyObject {
    var onPartial: ((String) -> Void)? { get set }
    var onFinal: ((String) -> Void)? { get set }
    var onStatus: ((String, Bool) -> Void)? { get set }
    var onError: ((String) -> Void)? { get set }
    var onPlaybackEnd: (() -> Void)? { get set }
    var onLevel: ((Float) -> Void)? { get set }
    func start() async throws
    func end()
    func pause()
    func setMuted(_ value: Bool)
    func speak(_ text: String)
    func interrupt()
    func resume() throws
    func setSpeaker(_ value: Bool) throws
}

struct AssistantVoiceAudioError: LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

/// Filters only substantial verbatim playback echoes. Short user commands such
/// as 停止 remain eligible even when the greeting mentions them.
struct AssistantVoiceEchoGate {
    static func normalized(_ value: String) -> String {
        value.filter { !$0.isWhitespace && !$0.isPunctuation }.lowercased()
    }
    static func isEcho(_ heard: String, playback: String) -> Bool {
        let heard = normalized(heard), playback = normalized(playback)
        return heard.count >= 6 && playback.contains(heard)
    }
}

private final class VoiceBufferSink: @unchecked Sendable {
    private let lock = NSLock()
    private var request: SFSpeechAudioBufferRecognitionRequest?
    func replace(_ request: SFSpeechAudioBufferRecognitionRequest?) {
        lock.lock(); self.request = request; lock.unlock()
    }
    func append(_ buffer: AVAudioPCMBuffer) {
        lock.lock(); defer { lock.unlock() }
        request?.append(buffer)
    }
}

/// Buffers from the synthesizer cross executors only as immutable owned copies.
private final class VoicePCM: @unchecked Sendable {
    let buffer: AVAudioPCMBuffer
    init?(_ source: AVAudioPCMBuffer) {
        guard let copy = AVAudioPCMBuffer(pcmFormat: source.format, frameCapacity: source.frameLength) else { return nil }
        copy.frameLength = source.frameLength
        let input = UnsafeMutableAudioBufferListPointer(source.mutableAudioBufferList)
        let output = UnsafeMutableAudioBufferListPointer(copy.mutableAudioBufferList)
        for index in input.indices {
            guard let from = input[index].mData, let to = output[index].mData else { return nil }
            memcpy(to, from, Int(input[index].mDataByteSize))
        }
        buffer = copy
    }
}

/// Microphone and synthesized playback share a voice-processing audio graph.
/// Recognition rotates without stopping the engine, including during replies.
@MainActor
final class NativeAssistantVoiceAudio: AssistantVoiceAudio {
    var onPartial: ((String) -> Void)?
    var onFinal: ((String) -> Void)?
    var onStatus: ((String, Bool) -> Void)?
    var onError: ((String) -> Void)?
    var onPlaybackEnd: (() -> Void)?
    var onLevel: ((Float) -> Void)?

    private var engine = AVAudioEngine()
    private var player = AVAudioPlayerNode()
    private var synthesizer = AVSpeechSynthesizer()
    private let sink = VoiceBufferSink()
    private var recognizer: SFSpeechRecognizer?
    private var recognition: SFSpeechRecognitionTask?
    private var recognitionID = UUID()
    private var playbackID = UUID()
    private var sessionID = UUID()
    private var debounce: Task<Void, Never>?
    private var rotation: Task<Void, Never>?
    private var recovery: Task<Void, Never>?
    private var speechRun: Task<Void, Never>?
    private var observations = Set<AnyCancellable>()
    private var active = false
    private var muted = false
    private var paused = false
    private var hasTap = false
    private var transcript = ""
    private var playback = ""
    private var echoText = ""
    private var echoUntil = Date.distantPast
    private var retryCount = 0
    private var recognitionStartedAt = Date.distantPast
    private var preferLocal = true
    private var usingLocal = false
    private var lastVoiceAt = Date.distantPast
    private var firstTextAt = Date.distantPast
    private var lastLevelAt = Date.distantPast
    private var outputSpeaker = true
    private var outstanding = 0
    private var producedAll = false
    private var converter: AVAudioConverter?
    private let outputFormat = AVAudioFormat(standardFormatWithSampleRate: 48_000, channels: 1)!

    init() {
        NotificationCenter.default.publisher(for: AVAudioSession.interruptionNotification).sink { @Sendable [weak self] note in
            let type = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
            let options = note.userInfo?[AVAudioSessionInterruptionOptionKey] as? UInt ?? 0
            Task { @MainActor in self?.interrupted(type: type, options: options) }
        }.store(in: &observations)
        NotificationCenter.default.publisher(for: AVAudioSession.routeChangeNotification).sink { @Sendable [weak self] note in
            let reason = note.userInfo?[AVAudioSessionRouteChangeReasonKey] as? UInt
            Task { @MainActor in
                guard let self, self.active, !self.paused,
                      reason == AVAudioSession.RouteChangeReason.oldDeviceUnavailable.rawValue || reason == AVAudioSession.RouteChangeReason.newDeviceAvailable.rawValue else { return }
                self.rebuild()
            }
        }.store(in: &observations)
        NotificationCenter.default.publisher(for: AVAudioSession.mediaServicesWereResetNotification).sink { @Sendable [weak self] _ in
            Task { @MainActor in self?.rebuild() }
        }.store(in: &observations)
    }

    func start() async throws {
        let token = UUID()
        sessionID = token
        let speech = await withCheckedContinuation { continuation in
            SFSpeechRecognizer.requestAuthorization { @Sendable status in continuation.resume(returning: status == .authorized) }
        }
        guard sessionID == token else { throw CancellationError() }
        let microphone = await withCheckedContinuation { continuation in
            AVAudioSession.sharedInstance().requestRecordPermission { @Sendable allowed in continuation.resume(returning: allowed) }
        }
        guard sessionID == token else { throw CancellationError() }
        guard speech, microphone else { throw AssistantVoiceAudioError(message: "请在系统设置中允许麦克风和语音识别。") }
        recognizer = SFSpeechRecognizer(locale: Locale(identifier: "zh-CN"))
        guard recognizer?.isAvailable == true else { throw AssistantVoiceAudioError(message: "中文语音识别暂不可用，请稍后重连。") }
        active = true
        muted = false
        paused = false
        retryCount = 0
        preferLocal = true
        outputSpeaker = true
        do { try configureGraph(); listen() }
        catch { end(); throw error }
    }

    private func configureGraph() throws {
        let audio = AVAudioSession.sharedInstance()
        try audio.setCategory(.playAndRecord, mode: .voiceChat, options: [.defaultToSpeaker, .allowBluetooth])
        try audio.setActive(true)
        if !outputSpeaker { try audio.overrideOutputAudioPort(.none) }
        // Enable before attaching taps or starting the engine. No silent
        // fallback to an unprocessed microphone: that could execute our reply.
        try engine.inputNode.setVoiceProcessingEnabled(true)
        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: outputFormat)
        let format = engine.inputNode.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0 else { throw AssistantVoiceAudioError(message: "麦克风不可用，请检查音频设备。") }
        let sink = self.sink
        engine.inputNode.installTap(onBus: 0, bufferSize: 1024, format: format) { @Sendable [weak self] buffer, _ in
            sink.append(buffer)
            let count = Int(buffer.frameLength)
            var level: Float = 0
            if let values = buffer.floatChannelData?.pointee, count > 0 {
                for index in 0..<count { level += values[index] * values[index] }
                level = sqrt(level / Float(count))
            }
            let value = level
            Task { @MainActor in self?.receivedLevel(value) }
        }
        hasTap = true
        engine.prepare()
        try engine.start()
    }

    private func receivedLevel(_ value: Float) {
        guard active, !paused else { return }
        let now = Date()
        if value > 0.015 { lastVoiceAt = now }
        if now.timeIntervalSince(lastLevelAt) > 0.1 {
            lastLevelAt = now
            onLevel?(muted ? 0 : min(1, value * 12))
        }
    }

    func end() {
        sessionID = UUID()
        active = false
        paused = false
        recovery?.cancel(); recovery = nil
        stopRecognition()
        interrupt()
        engine.stop()
        if hasTap { engine.inputNode.removeTap(onBus: 0); hasTap = false }
        engine = AVAudioEngine()
        player = AVAudioPlayerNode()
        synthesizer = AVSpeechSynthesizer()
        onLevel?(0)
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
    }

    func pause() {
        paused = true
        recovery?.cancel(); recovery = nil
        stopRecognition()
        interrupt()
        engine.stop()
        try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
        onLevel?(0)
    }

    func setMuted(_ value: Bool) {
        muted = value
        engine.inputNode.isVoiceProcessingInputMuted = value
        if value { stopRecognition(); onLevel?(0) }
        else { listen() }
    }

    func setSpeaker(_ value: Bool) throws {
        try AVAudioSession.sharedInstance().overrideOutputAudioPort(value ? .speaker : .none)
        outputSpeaker = value
    }

    func resume() throws {
        guard active else { return }
        paused = false
        try AVAudioSession.sharedInstance().setActive(true)
        if !engine.isRunning { try engine.start() }
        listen()
        onStatus?("正在听", false)
    }

    private func interrupted(type: UInt?, options: UInt) {
        guard active else { return }
        if type == AVAudioSession.InterruptionType.began.rawValue {
            paused = true
            recovery?.cancel()
            stopRecognition()
            interrupt()
            engine.stop()
            onStatus?("通话被系统暂停", true)
        } else if type == AVAudioSession.InterruptionType.ended.rawValue {
            if AVAudioSession.InterruptionOptions(rawValue: options).contains(.shouldResume) {
                do { try resume() } catch { onError?("音频恢复失败，请点击重新连接。") }
            } else { onStatus?("点击继续通话", true) }
        }
    }

    private func rebuild() {
        guard active else { return }
        stopRecognition()
        interrupt()
        engine.stop()
        if hasTap { engine.inputNode.removeTap(onBus: 0); hasTap = false }
        engine = AVAudioEngine(); player = AVAudioPlayerNode(); synthesizer = AVSpeechSynthesizer()
        do {
            try configureGraph()
            engine.inputNode.isVoiceProcessingInputMuted = muted
            if !paused { listen() }
        } catch { onError?("音频设备连接失败，请重新连接通话。") }
    }

    private func listen() {
        guard active, !muted, !paused else { return }
        stopRecognition()
        let token = recognitionID
        transcript = ""
        firstTextAt = .distantPast
        let request = SFSpeechAudioBufferRecognitionRequest()
        request.shouldReportPartialResults = true
        // Keep audio local when Chinese on-device recognition is installed.
        usingLocal = preferLocal && recognizer?.supportsOnDeviceRecognition == true
        if usingLocal { request.requiresOnDeviceRecognition = true }
        recognitionStartedAt = Date()
        sink.replace(request)
        recognition = recognizer?.recognitionTask(with: request) { @Sendable [weak self] result, error in
            let text = result?.bestTranscription.formattedString
            let final = result?.isFinal == true
            let failure = error != nil
            Task { @MainActor in self?.recognized(text, final: final, failure: failure, token: token) }
        }
        rotation = Task { [weak self] in
            try? await Task.sleep(for: .seconds(45))
            guard !Task.isCancelled, let self, self.recognitionID == token else { return }
            if self.transcript.isEmpty { self.listen() } else { self.deliver() }
        }
    }

    private func recognized(_ text: String?, final: Bool, failure: Bool, token: UUID) {
        guard active, !paused, !muted, recognitionID == token else { return }
        if let text, !text.isEmpty {
            if (!playback.isEmpty || Date() < echoUntil), AssistantVoiceEchoGate.isEcho(text, playback: playback.isEmpty ? echoText : playback) {
                if final { listen() }
                return
            }
            retryCount = 0
            if text != transcript {
                if transcript.isEmpty { firstTextAt = Date() }
                transcript = text
                onPartial?(text)
                debounce?.cancel()
                debounce = Task { [weak self] in
                    try? await Task.sleep(for: .milliseconds(900))
                    guard !Task.isCancelled, let self, self.recognitionID == token else { return }
                    // Do not cut off someone still speaking just because the
                    // recognizer hasn't emitted a new partial yet.
                    while Date().timeIntervalSince(self.lastVoiceAt) < 0.55 && Date().timeIntervalSince(self.firstTextAt) < 30 {
                        try? await Task.sleep(for: .milliseconds(150))
                        guard !Task.isCancelled, self.recognitionID == token else { return }
                    }
                    self.deliver()
                }
            }
            if final { deliver(); return }
        } else if final { listen(); return }
        if failure { recoverRecognition() }
    }

    private func deliver() {
        let text = transcript.trimmingCharacters(in: .whitespacesAndNewlines)
        listen() // New recognition generation rejects late callbacks.
        if !text.isEmpty { onFinal?(text) }
    }

    private func stopRecognition() {
        recognitionID = UUID()
        debounce?.cancel(); debounce = nil
        rotation?.cancel(); rotation = nil
        sink.replace(nil)
        recognition?.cancel(); recognition = nil
    }

    private func recoverRecognition() {
        let lived = Date().timeIntervalSince(recognitionStartedAt)
        stopRecognition()
        // A recognizer that ran for a while and ended during silence should
        // not exhaust the rapid-failure budget of a long phone conversation.
        if lived >= 12 { retryCount = 0 }
        retryCount += 1
        if retryCount == 2 && usingLocal {
            // supportsOnDeviceRecognition does not guarantee installed assets.
            // Speech permission already discloses possible Apple processing.
            preferLocal = false
        }
        guard retryCount <= 3 else {
            onError?("语音识别连接中断，请重新连接通话。")
            return
        }
        onStatus?("正在恢复语音连接", true)
        let token = sessionID
        let delay = Double(retryCount)
        recovery?.cancel()
        recovery = Task { [weak self] in
            try? await Task.sleep(for: .seconds(delay))
            guard !Task.isCancelled, let self, self.active, self.sessionID == token else { return }
            self.listen()
            self.onStatus?(self.muted ? "麦克风已静音" : "正在听", false)
        }
    }

    func interrupt() {
        playbackID = UUID()
        speechRun?.cancel(); speechRun = nil
        synthesizer.stopSpeaking(at: .immediate)
        player.stop()
        if !playback.isEmpty { echoText = playback; echoUntil = Date().addingTimeInterval(0.6) }
        playback = ""
        converter = nil
        outstanding = 0
        producedAll = false
    }

    func speak(_ text: String) {
        guard active, !paused else { return }
        interrupt()
        playback = String(text.prefix(500))
        let token = playbackID
        let utterance = AVSpeechUtterance(string: playback)
        utterance.voice = AVSpeechSynthesisVoice(language: "zh-CN")
        let stream = AsyncStream<VoicePCM> { continuation in
            synthesizer.write(utterance) { @Sendable buffer in
                guard let pcm = buffer as? AVAudioPCMBuffer else { continuation.finish(); return }
                if pcm.frameLength == 0 { continuation.finish() }
                else if let packet = VoicePCM(pcm) { continuation.yield(packet) }
            }
        }
        speechRun = Task { [weak self] in
            guard let self else { return }
            for await packet in stream {
                guard !Task.isCancelled, self.playbackID == token else { return }
                do { try self.schedule(packet.buffer, token: token) }
                catch { self.interrupt(); self.onError?("语音播报失败，请重新连接通话。"); return }
            }
            guard !Task.isCancelled, self.playbackID == token else { return }
            self.producedAll = true
            self.finishedPlayback(token)
        }
    }

    private func schedule(_ input: AVAudioPCMBuffer, token: UUID) throws {
        if converter == nil { converter = AVAudioConverter(from: input.format, to: outputFormat) }
        guard let converter, converter.inputFormat == input.format,
              let output = AVAudioPCMBuffer(pcmFormat: outputFormat, frameCapacity: AVAudioFrameCount(Double(input.frameLength) * outputFormat.sampleRate / input.format.sampleRate + 256)) else {
            throw AssistantVoiceAudioError(message: "音频格式不支持。")
        }
        let supplied = LockedState(initialState: false)
        guard let ownedInput = VoicePCM(input) else { throw AssistantVoiceAudioError(message: "音频缓冲区不可用。") }
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { @Sendable _, state in
            if supplied.withLock({ value in let previous = value; value = true; return previous }) {
                state.pointee = .noDataNow; return nil
            }
            state.pointee = .haveData
            return ownedInput.buffer
        }
        guard status != .error, error == nil else { throw error ?? AssistantVoiceAudioError(message: "音频转换失败。") }
        guard output.frameLength > 0 else { return }
        outstanding += 1
        player.scheduleBuffer(output, completionCallbackType: .dataPlayedBack) { @Sendable [weak self] _ in
            Task { @MainActor in
                guard let self, self.playbackID == token else { return }
                self.outstanding -= 1
                self.finishedPlayback(token)
            }
        }
        if !player.isPlaying { player.play() }
    }

    private func finishedPlayback(_ token: UUID) {
        guard playbackID == token, producedAll, outstanding == 0 else { return }
        echoText = playback
        echoUntil = Date().addingTimeInterval(0.6)
        playback = ""
        onPlaybackEnd?()
    }
}
