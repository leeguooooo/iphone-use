import UIKit
import Photos
import CryptoKit
import ImageIO

@main
final class CaptureFixture: UIResponder, UIApplicationDelegate {
  var window: UIWindow?
  private var runID = ""
  private var bits: [Bool] = []
  private var points: [CGPoint] = []
  private var baseline = Set<String>()
  private var started = Date()
  private var reading = false
  private var timer: Timer?
  private let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]

  private func write(_ name: String, _ values: [String: Any]) {
    var body = values
    body["run_id"] = runID
    body["pid"] = ProcessInfo.processInfo.processIdentifier
    if let data = try? JSONSerialization.data(withJSONObject: body, options: [.sortedKeys]) {
      try? data.write(to: documents.appendingPathComponent(name), options: .atomic)
    }
    if name == "capture-photo.json" {
      DispatchQueue.main.async {
        guard let window = self.window, let view = window.rootViewController?.view else { return }
        let label = UILabel(frame: CGRect(x: 20, y: window.bounds.height - 140, width: window.bounds.width - 40, height: 60))
        label.accessibilityIdentifier = "capture-proof"
        label.text = values["verified"] as? Bool == true ? "VERIFIED" : "FAILED"
        label.textColor = .black
        view.addSubview(label)
      }
    }
  }

  func application(_ application: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?) -> Bool {
    guard let marker = ProcessInfo.processInfo.arguments.last, UUID(uuidString: marker) != nil else { return false }
    runID = marker
    guard PHPhotoLibrary.authorizationStatus(for: .readWrite) == .authorized else {
      write("capture-ready.json", ["ready": false, "error": "full Photos access was not granted to this isolated fixture"])
      return true
    }
    started = Date()
    PHAsset.fetchAssets(with: .image, options: nil).enumerateObjects { asset, _, _ in self.baseline.insert(asset.localIdentifier) }
    bits = runID.replacingOccurrences(of: "-", with: "").flatMap { character -> [Bool] in
      let nibble = Int(String(character), radix: 16)!
      return (0..<4).reversed().map { nibble & (1 << $0) != 0 }
    }
    let controller = UIViewController()
    controller.view.backgroundColor = .white
    let window = UIWindow(frame: UIScreen.main.bounds)
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
      let point = window.convert(view.bounds.center, from: view)
      points.append(CGPoint(x: point.x / window.bounds.width, y: point.y / window.bounds.height))
    }
    write("capture-ready.json", ["ready": true, "baseline_assets": baseline.count, "marker_cells": bits.count,
      "screen_width": Int(UIScreen.main.nativeBounds.width), "screen_height": Int(UIScreen.main.nativeBounds.height)])
    timer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in self?.inspectNewAsset() }
    return true
  }

  private func inspectNewAsset() {
    guard !reading else { return }
    var assets: [PHAsset] = []
    PHAsset.fetchAssets(with: .image, options: nil).enumerateObjects { asset, _, _ in
      if !self.baseline.contains(asset.localIdentifier) { assets.append(asset) }
    }
    guard !assets.isEmpty else { return }
    reading = true
    timer?.invalidate()
    guard assets.count == 1, let asset = assets.first, asset.mediaSubtypes.contains(.photoScreenshot),
          let date = asset.creationDate, date >= started else {
      write("capture-photo.json", ["verified": false, "new_assets": assets.count, "error": "expected exactly one new system screenshot after fixture start"])
      return
    }
    let options = PHImageRequestOptions()
    options.isNetworkAccessAllowed = false
    options.version = .original
    options.deliveryMode = .highQualityFormat
    PHImageManager.default().requestImageDataAndOrientation(for: asset, options: options) { data, _, _, _ in
      guard let data = data else {
        self.write("capture-photo.json", ["verified": false, "error": "new Photos asset has no locally readable original bytes"])
        return
      }
      let result = self.verifyPixels(data)
      let negative = self.verifyPixels(data, inverted: true)
      self.write("capture-photo.json", ["verified": result.matched == 128 && negative.matched < 128, "new_assets": assets.count,
        "screenshot_subtype": true, "pixel_width": asset.pixelWidth, "pixel_height": asset.pixelHeight,
        "marker_cells": self.bits.count, "matched_cells": result.matched, "negative_matched_cells": negative.matched, "row_order": result.order,
        "photo_sha256": SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined(),
        "asset_id_sha256": SHA256.hash(data: Data(asset.localIdentifier.utf8)).map { String(format: "%02x", $0) }.joined()])
    }
  }

  private func verifyPixels(_ data: Data, inverted: Bool = false) -> (matched: Int, order: String) {
    guard let source = CGImageSourceCreateWithData(data as CFData, nil), let image = CGImageSourceCreateImageAtIndex(source, 0, nil),
          image.width > 0, image.height > 0, image.width * image.height <= 20000000 else { return (0, "unreadable") }
    var pixels = [UInt8](repeating: 0, count: image.width * image.height * 4)
    return pixels.withUnsafeMutableBytes { raw -> (Int, String) in
      guard let context = CGContext(data: raw.baseAddress, width: image.width, height: image.height, bitsPerComponent: 8,
        bytesPerRow: image.width * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue) else { return (0, "decode_failed") }
      context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
      func count(flipped: Bool) -> Int {
        points.enumerated().filter { index, point in
          let x = min(image.width - 1, max(0, Int(point.x * CGFloat(image.width))))
          let originalY = min(image.height - 1, max(0, Int(point.y * CGFloat(image.height))))
          let y = flipped ? image.height - 1 - originalY : originalY
          let offset = (y * image.width + x) * 4
          let red = Int(raw[offset]), blue = Int(raw[offset + 2])
          return bits[index] != inverted ? blue > red + 100 : red > blue + 100
        }.count
      }
      let normal = count(flipped: false), flipped = count(flipped: true)
      return normal >= flipped ? (normal, "decoded") : (flipped, "reversed_rows")
    }
  }
}

private extension CGRect {
  var center: CGPoint { CGPoint(x: midX, y: midY) }
}
