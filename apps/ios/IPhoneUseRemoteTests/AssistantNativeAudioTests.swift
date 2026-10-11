import XCTest
import AVFoundation
@testable import IPhoneUseRemote

@MainActor
final class AssistantNativeAudioTests: XCTestCase {
    /// Run only on an explicitly selected QA device/app. No model requests,
    /// task submissions, microphone audio files, or transcript logging.
    func testNativeDuplexPlaybackRotationAndMute() async throws {
        guard ProcessInfo.processInfo.environment["VOICE_NATIVE_AUDIO_TEST"] == "1" else {
            throw XCTSkip("Requires a QA device and explicit microphone opt-in")
        }
        let audio = NativeAssistantVoiceAudio()
        var levels = 0
        var errors: [String] = []
        var recognized = 0
        var finished = 0
        audio.onLevel = { _ in levels += 1 }
        audio.onError = { errors.append($0) }
        audio.onFinal = { _ in recognized += 1 }
        audio.onPlaybackEnd = { finished += 1 }
        defer { audio.end() }
        try await audio.start()
        audio.speak("语音通话测试。麦克风和播放正在同时工作。")
        for _ in 0..<150 {
            if finished > 0 || !errors.isEmpty { break }
            try await Task.sleep(for: .milliseconds(100))
        }
        XCTAssertEqual(finished, 1, "Synthesized speech must finish playing through the voice-processing graph")
        XCTAssertTrue(errors.isEmpty, "Native audio must remain healthy")
        XCTAssertGreaterThan(levels, 5, "Capture must continue during playback")
        // The callback must not recognize the assistant's own playback as a
        // user turn. This is acoustic evidence, not a mocked echo-filter test.
        XCTAssertEqual(recognized, 0, "Assistant playback must not become a user command")
        audio.setMuted(true)
        try await Task.sleep(for: .seconds(1))
        audio.setMuted(false)
        print("VOICE_QA_READY_FOR_BACKGROUND")
        let previousLevels = levels
        // Cross the 45-second Speech rotation while continuously capturing.
        try await Task.sleep(for: .seconds(48))
        XCTAssertGreaterThan(levels, previousLevels + 100, "Capture must survive rotation and backgrounding")
        XCTAssertTrue(errors.isEmpty, "Recognition must survive the idle period")
        audio.speak("通话测试完成。")
        for _ in 0..<100 {
            if finished == 2 || !errors.isEmpty { break }
            try await Task.sleep(for: .milliseconds(100))
        }
        XCTAssertEqual(finished, 2)
        XCTAssertTrue(errors.isEmpty)
    }
}
