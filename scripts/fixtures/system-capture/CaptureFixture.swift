import UIKit

@main
final class CaptureFixture: UIResponder, UIApplicationDelegate {
  func application(_ application: UIApplication, configurationForConnecting session: UISceneSession,
                   options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let configuration = UISceneConfiguration(name: "Default", sessionRole: session.role)
    configuration.delegateClass = CaptureScene.self
    return configuration
  }
}

private final class CaptureController: UIViewController {
  override var supportedInterfaceOrientations: UIInterfaceOrientationMask { .portrait }
}

@objc(CaptureScene)
final class CaptureScene: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?

  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene,
          let runID = ProcessInfo.processInfo.arguments.last, UUID(uuidString: runID) != nil else { return }
    let bits = runID.replacingOccurrences(of: "-", with: "").flatMap { character -> [Bool] in
      let nibble = Int(String(character), radix: 16)!
      return (0..<4).reversed().map { nibble & (1 << $0) != 0 }
    }
    let controller = CaptureController()
    controller.view.backgroundColor = .white
    let window = UIWindow(windowScene: scene)
    window.rootViewController = controller
    self.window = window
    window.makeKeyAndVisible()
    let label = UILabel(frame: CGRect(x: 20, y: 90, width: window.bounds.width - 40, height: 80))
    label.text = runID
    label.accessibilityIdentifier = "capture-marker"
    label.font = .monospacedSystemFont(ofSize: 19, weight: .bold)
    label.textColor = .black
    label.adjustsFontSizeToFitWidth = true
    controller.view.addSubview(label)
    let cell = min((window.bounds.width - 40) / 16, 20)
    for (index, bit) in bits.enumerated() {
      let frame = CGRect(x: 20 + CGFloat(index % 16) * cell, y: 200 + CGFloat(index / 16) * cell, width: cell, height: cell)
      let view = UIView(frame: frame.insetBy(dx: 1, dy: 1))
      view.backgroundColor = bit ? .blue : .red
      controller.view.addSubview(view)
    }
    let ready: [String: Any] = ["ready": true, "run_id": runID, "pid": ProcessInfo.processInfo.processIdentifier,
      "marker_cells": bits.count, "screen_width": Int(UIScreen.main.nativeBounds.width),
      "screen_height": Int(UIScreen.main.nativeBounds.height), "screen_points_width": Double(window.bounds.width),
      "screen_points_height": Double(window.bounds.height), "cell_points": Double(cell), "grid_x": 20, "grid_y": 200]
    let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    if let data = try? JSONSerialization.data(withJSONObject: ready, options: [.sortedKeys]) {
      try? data.write(to: documents.appendingPathComponent("capture-ready.json"), options: .atomic)
    }
  }
}
