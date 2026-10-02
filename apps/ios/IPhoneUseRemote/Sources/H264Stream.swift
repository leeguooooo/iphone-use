import AVFoundation
import CoreMedia
import UIKit

/// One message of the daemon's `/agent/h264` stream:
/// `[u32 BE length of the rest][u8 flags][u64 BE pts µs][Annex-B access unit]`.
struct H264Message {
    let keyframe: Bool
    let ptsMicros: UInt64
    let annexB: Data
}

/// Incremental parser for the framed stream.
struct H264MessageParser {
    private var buffer = Data()

    mutating func push(_ chunk: Data) -> [H264Message] {
        buffer.append(chunk)
        var out: [H264Message] = []
        while buffer.count >= 4 {
            let start = buffer.startIndex
            let length = Int(buffer[start]) << 24 | Int(buffer[start + 1]) << 16
                | Int(buffer[start + 2]) << 8 | Int(buffer[start + 3])
            guard length >= 9, length < 16 << 20 else {
                buffer.removeAll() // corrupt framing: resync on the next connection
                break
            }
            guard buffer.count >= 4 + length else { break }
            let flags = buffer[start + 4]
            var pts: UInt64 = 0
            for i in 0..<8 { pts = pts << 8 | UInt64(buffer[start + 5 + i]) }
            let payload = buffer.subdata(in: (start + 13)..<(start + 4 + length))
            out.append(H264Message(keyframe: flags & 1 != 0, ptsMicros: pts, annexB: payload))
            buffer.removeSubrange(start..<(start + 4 + length))
        }
        return out
    }
}

/// Splits Annex-B into NAL units (start codes 00 00 01 / 00 00 00 01).
func annexBNALUnits(_ data: Data) -> [Data] {
    let bytes = [UInt8](data)
    var starts: [(index: Int, codeLength: Int)] = []
    var i = 0
    while i + 3 <= bytes.count {
        if bytes[i] == 0, bytes[i + 1] == 0 {
            if bytes[i + 2] == 1 {
                starts.append((i, 3)); i += 3; continue
            }
            if i + 4 <= bytes.count, bytes[i + 2] == 0, bytes[i + 3] == 1 {
                starts.append((i, 4)); i += 4; continue
            }
        }
        i += 1
    }
    var units: [Data] = []
    for (n, start) in starts.enumerated() {
        let from = start.index + start.codeLength
        let to = n + 1 < starts.count ? starts[n + 1].index : bytes.count
        if to > from { units.append(Data(bytes[from..<to])) }
    }
    return units
}

/// Decodes and shows the stream with the system's hardware decoder.
final class VideoDisplayView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }

    /// Pixel size of the decoded video, once known.
    private(set) var videoSize: CGSize = .zero
    var onVideoSize: ((CGSize) -> Void)?
    var onFrame: (() -> Void)?

    private var format: CMVideoFormatDescription?
    private var waitingForKeyframe = true

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not used") }

    /// The rectangle the video actually occupies inside this view.
    var videoRect: CGRect {
        guard videoSize.width > 0, videoSize.height > 0 else { return bounds }
        return AVMakeRect(aspectRatio: videoSize, insideRect: bounds)
    }

    func reset() {
        displayLayer.sampleBufferRenderer.flush(removingDisplayedImage: false) {}
        waitingForKeyframe = true
    }

    func enqueue(_ message: H264Message) {
        var parameterSets: [Data] = []
        var slices: [Data] = []
        for unit in annexBNALUnits(message.annexB) {
            switch unit.first.map({ $0 & 0x1F }) {
            case 7, 8: parameterSets.append(unit)
            case 9, 6: continue // access unit delimiter, SEI
            default: slices.append(unit)
            }
        }
        if parameterSets.count >= 2, let newFormat = makeFormat(parameterSets) {
            if format == nil || !CMFormatDescriptionEqual(format, otherFormatDescription: newFormat) {
                format = newFormat
                let dims = CMVideoFormatDescriptionGetDimensions(newFormat)
                videoSize = CGSize(width: Int(dims.width), height: Int(dims.height))
                onVideoSize?(videoSize)
            }
        }
        if waitingForKeyframe {
            guard message.keyframe, format != nil else { return }
            waitingForKeyframe = false
        }
        guard let format, !slices.isEmpty, let sample = makeSample(slices, format: format) else { return }
        let renderer = displayLayer.sampleBufferRenderer
        if renderer.status == .failed {
            renderer.flush(removingDisplayedImage: false) {}
            waitingForKeyframe = true
            return
        }
        renderer.enqueue(sample)
        onFrame?()
    }

    private func makeFormat(_ sets: [Data]) -> CMVideoFormatDescription? {
        let sps = sets.first { ($0.first ?? 0) & 0x1F == 7 }
        let pps = sets.first { ($0.first ?? 0) & 0x1F == 8 }
        guard let sps, let pps else { return nil }
        var format: CMVideoFormatDescription?
        let status = sps.withUnsafeBytes { spsBytes in
            pps.withUnsafeBytes { ppsBytes in
                let pointers = [spsBytes.bindMemory(to: UInt8.self).baseAddress!,
                                ppsBytes.bindMemory(to: UInt8.self).baseAddress!]
                let sizes = [sps.count, pps.count]
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: 2,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &format)
            }
        }
        return status == noErr ? format : nil
    }

    /// AVCC sample: every slice prefixed with its 4-byte length.
    private func makeSample(_ slices: [Data], format: CMVideoFormatDescription) -> CMSampleBuffer? {
        var avcc = Data()
        for slice in slices {
            var length = UInt32(slice.count).bigEndian
            avcc.append(Data(bytes: &length, count: 4))
            avcc.append(slice)
        }
        var block: CMBlockBuffer?
        let count = avcc.count
        var status = CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil, blockLength: count,
            blockAllocator: kCFAllocatorDefault, customBlockSource: nil, offsetToData: 0,
            dataLength: count, flags: 0, blockBufferOut: &block)
        guard status == kCMBlockBufferNoErr, let block else { return nil }
        status = avcc.withUnsafeBytes {
            CMBlockBufferReplaceDataBytes(with: $0.baseAddress!, blockBuffer: block,
                                          offsetIntoDestination: 0, dataLength: count)
        }
        guard status == kCMBlockBufferNoErr else { return nil }
        var sample: CMSampleBuffer?
        var sizes = [count]
        status = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault, dataBuffer: block, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 0, sampleTimingArray: nil,
            sampleSizeEntryCount: 1, sampleSizeArray: &sizes, sampleBufferOut: &sample)
        guard status == noErr, let sample else { return nil }
        // Live view: show each frame as it arrives, no presentation clock.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(dict,
                                 Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                                 Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        }
        return sample
    }
}

/// Reads `/agent/h264` and hands each message to a callback on the main queue.
/// Reconnects with backoff until stopped.
final class H264StreamReader: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private let request: URLRequest
    private let session: URLSession
    private var task: URLSessionDataTask?
    private var parser = H264MessageParser()
    private var stopped = true
    private var attempt = 0
    private let onMessage: @MainActor (H264Message) -> Void
    private let onState: @MainActor (Bool, String?) -> Void

    init(request: URLRequest,
         onMessage: @escaping @MainActor (H264Message) -> Void,
         onState: @escaping @MainActor (Bool, String?) -> Void) {
        self.request = request
        self.onMessage = onMessage
        self.onState = onState
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.timeoutIntervalForRequest = 10
        config.timeoutIntervalForResource = .infinity
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        session = URLSession(configuration: config, delegate: nil, delegateQueue: queue)
        super.init()
    }

    func start() {
        stopped = false
        connect()
    }

    func stop() {
        stopped = true
        task?.cancel()
        task = nil
    }

    private func connect() {
        guard !stopped else { return }
        parser = H264MessageParser()
        let task = session.dataTask(with: request)
        task.delegate = self
        self.task = task
        task.resume()
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask,
                    didReceive response: URLResponse) async -> URLSession.ResponseDisposition {
        let ok = (response as? HTTPURLResponse)?.statusCode == 200
        let code = (response as? HTTPURLResponse)?.statusCode ?? 0
        if ok { attempt = 0 }
        let onState = self.onState
        await MainActor.run { onState(ok, ok ? nil : "画面暂不可用（\(code)）") }
        return ok ? .allow : .cancel
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        let messages = parser.push(data)
        guard !messages.isEmpty else { return }
        let onMessage = self.onMessage
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                for message in messages { onMessage(message) }
            }
        }
    }

    func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard !stopped, task === self.task else { return }
        attempt += 1
        let delay = min(5.0, 0.4 * Double(attempt))
        let onState = self.onState
        DispatchQueue.main.async {
            MainActor.assumeIsolated { onState(false, "画面断开，正在重连…") }
        }
        DispatchQueue.global().asyncAfter(deadline: .now() + delay) { [weak self] in
            self?.connect()
        }
    }
}
