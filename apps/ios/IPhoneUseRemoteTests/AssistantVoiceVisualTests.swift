import XCTest
import SwiftUI
@testable import IPhoneUseRemote

@MainActor
private final class VoiceVisualAudio: AssistantVoiceAudio {
    var onPartial: ((String) -> Void)?
    var onFinal: ((String) -> Void)?
    var onStatus: ((String, Bool) -> Void)?
    var onError: ((String) -> Void)?
    var onPlaybackEnd: (() -> Void)?
    var onLevel: ((Float) -> Void)?
    func start() async throws {}
    func end() {}
    func pause() {}
    func setMuted(_ value: Bool) {}
    func speak(_ text: String) {}
    func interrupt() {}
    func resume() throws {}
    func setSpeaker(_ value: Bool) throws {}
}

@MainActor
private final class VoiceVisualPhone: AssistantPhoneControlling {
    var state = AssistantPhoneState()
    var models: [ModelsPayload.Assist] = []
    var onChange: (() -> Void)?
    func start(goal: String) async -> Bool { false }
    func stop() async {}
    func disconnect() {}
}

@MainActor
final class AssistantVoiceVisualTests: XCTestCase {
    func testCompactAndStandardCallLayouts() async throws {
        let audio = VoiceVisualAudio(), phone = VoiceVisualPhone()
        let call = AssistantVoiceCall(audio: audio)
        let defaults = UserDefaults(suiteName: "voice-visual-\(UUID().uuidString)")!
        let agent = AgentController(defaults: defaults)
        await call.start(phone: phone)
        defer { call.end() }
        try await capture(call, agent: agent, size: CGSize(width: 320, height: 568), style: .light, name: "voice-compact")
        try await capture(call, agent: agent, size: CGSize(width: 390, height: 844), style: .dark, name: "voice-dark")
        audio.onError?("语音识别连接中断，请重新连接通话。")
        try await capture(call, agent: agent, size: CGSize(width: 390, height: 844), style: .light, name: "voice-reconnect")
    }
    private func capture(_ call: AssistantVoiceCall, agent: AgentController, size: CGSize,
                         style: UIUserInterfaceStyle, name: String) async throws {
        let controller = UIHostingController(rootView: AssistantVoiceCallView(call: call, agent: agent))
        let previousWindow = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first?.windows.first(where: \.isKeyWindow)
        let window: UIWindow
        if let scene = UIApplication.shared.connectedScenes.compactMap({ $0 as? UIWindowScene }).first {
            window = UIWindow(windowScene: scene)
            window.frame = CGRect(origin: .zero, size: size)
        } else { window = UIWindow(frame: CGRect(origin: .zero, size: size)) }
        window.overrideUserInterfaceStyle = style
        window.rootViewController = controller
        window.makeKeyAndVisible()
        controller.view.frame = window.bounds
        controller.view.setNeedsLayout()
        controller.view.layoutIfNeeded()
        try await Task.sleep(for: .milliseconds(200))
        let image = UIGraphicsImageRenderer(size: size).image { context in
            controller.view.layer.render(in: context.cgContext)
        }
        let attachment = XCTAttachment(image: image)
        attachment.name = name
        attachment.lifetime = .keepAlways
        add(attachment)
        window.isHidden = true
        previousWindow?.makeKeyAndVisible()
    }
}
