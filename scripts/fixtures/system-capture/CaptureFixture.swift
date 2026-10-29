import UIKit
import Photos
import PhotosUI
import UniformTypeIdentifiers

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
final class CaptureScene: UIResponder, UIWindowSceneDelegate, PHPickerViewControllerDelegate {
  var window: UIWindow?
  var markerRunID = ""

  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene,
          let runID = ProcessInfo.processInfo.arguments.last, UUID(uuidString: runID) != nil else { return }
    markerRunID = runID
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
    let pick = UIButton(type: .system)
    pick.frame = CGRect(x: 20, y: 500, width: window.bounds.width - 40, height: 60)
    pick.setTitle("读取已保存的截图", for: .normal)
    pick.addTarget(self, action: #selector(openSavedScreenshot), for: .touchUpInside)
    controller.view.addSubview(pick)
    let ready: [String: Any] = ["ready": true, "run_id": runID, "time": Date().timeIntervalSince1970, "pid": ProcessInfo.processInfo.processIdentifier,
      "marker_cells": bits.count, "screen_width": Int(UIScreen.main.nativeBounds.width),
      "screen_height": Int(UIScreen.main.nativeBounds.height), "screen_points_width": Double(window.bounds.width),
      "screen_points_height": Double(window.bounds.height), "cell_points": Double(cell), "grid_x": 20, "grid_y": 200]
    let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    if let data = try? JSONSerialization.data(withJSONObject: ready, options: [.sortedKeys]) {
      try? data.write(to: documents.appendingPathComponent("capture-ready.json"), options: .atomic)
    }
  }
  @objc func openSavedScreenshot() {
    var configuration = PHPickerConfiguration(photoLibrary: .shared())
    configuration.filter = .screenshots
    configuration.selectionLimit = 1
    configuration.preferredAssetRepresentationMode = .current
    let picker = PHPickerViewController(configuration: configuration)
    picker.delegate = self
    window?.rootViewController?.present(picker, animated: true)
  }

  func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
    picker.dismiss(animated: true)
    let runID = markerRunID
    guard results.count == 1, let selected = results.first,
          let type = selected.itemProvider.registeredTypeIdentifiers.first(where: {
            UTType($0)?.conforms(to: .image) == true
          }) else { return }
    selected.itemProvider.loadFileRepresentation(forTypeIdentifier: type) { url, error in
      let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
      var receipt: [String: Any] = ["run_id": runID, "scope": "system_picker_single_screenshot",
        "representation": "current", "asset_identifier": selected.assetIdentifier as Any? ?? NSNull(),
        "type_identifier": type, "copied": false]
      do {
        guard let url else { throw error ?? NSError(domain: "CaptureFixture", code: 1) }
        let name = "picker-selected-" + runID + ".bin"
        try FileManager.default.copyItem(at: url, to: documents.appendingPathComponent(name))
        receipt["file"] = name
        receipt["copied"] = true
      } catch { receipt["error"] = String(describing: error) }
      if let data = try? JSONSerialization.data(withJSONObject: receipt, options: [.sortedKeys]) {
        try? data.write(to: documents.appendingPathComponent("picker-result.json"), options: .atomic)
      }
    }
  }

}
