import Foundation
import CryptoKit
import CoreGraphics
import ImageIO

// Runs on the Mac, independently of the rendering App and native request.
// It reads only the copied new asset and its public synthetic layout receipt.
let arguments = CommandLine.arguments
precondition(arguments.count == 3)
let data = try Data(contentsOf: URL(fileURLWithPath: arguments[1]))
let readyData = try Data(contentsOf: URL(fileURLWithPath: arguments[2]))
let ready = try JSONSerialization.jsonObject(with: readyData) as! [String: Any]
let runID = ready["run_id"] as! String
precondition(UUID(uuidString: runID) != nil)
let expected = runID.replacingOccurrences(of: "-", with: "").flatMap { character -> [Bool] in
  let nibble = Int(String(character), radix: 16)!
  return (0..<4).reversed().map { nibble & (1 << $0) != 0 }
}
let source = CGImageSourceCreateWithData(data as CFData, nil)!
precondition(CGImageSourceGetStatus(source) == .statusComplete)
let image = CGImageSourceCreateImageAtIndex(source, 0, nil)!
precondition(image.width > 0 && image.height > 0 && image.width * image.height <= 20000000)
let pointWidth = (ready["screen_points_width"] as! NSNumber).doubleValue
let pointHeight = (ready["screen_points_height"] as! NSNumber).doubleValue
let cell = (ready["cell_points"] as! NSNumber).doubleValue
let gridX = (ready["grid_x"] as! NSNumber).doubleValue
let gridY = (ready["grid_y"] as! NSNumber).doubleValue
var pixels = [UInt8](repeating: 0, count: image.width * image.height * 4)
let counts = pixels.withUnsafeMutableBytes { raw -> (Int, Int) in
  let context = CGContext(data: raw.baseAddress, width: image.width, height: image.height, bitsPerComponent: 8,
    bytesPerRow: image.width * 4, space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue | CGBitmapInfo.byteOrder32Big.rawValue)!
  context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
  func count(inverted: Bool) -> Int {
    expected.indices.filter { index in
      let px = (gridX + (Double(index % 16) + 0.5) * cell) / pointWidth
      let py = (gridY + (Double(index / 16) + 0.5) * cell) / pointHeight
      let x = min(image.width - 1, max(0, Int(px * Double(image.width))))
      let y = min(image.height - 1, max(0, Int(py * Double(image.height))))
      let offset = (y * image.width + x) * 4
      let red = Int(raw[offset]), blue = Int(raw[offset + 2])
      return expected[index] != inverted ? blue > red + 100 : red > blue + 100
    }.count
  }
  return (count(inverted: false), count(inverted: true))
}
let dimensionsMatch = image.width == (ready["screen_width"] as! Int) && image.height == (ready["screen_height"] as! Int)
let result: [String: Any] = ["run_id": runID, "verified": counts.0 == 128 && counts.1 < 128 && dimensionsMatch,
  "marker_cells": expected.count, "matched_cells": counts.0, "negative_matched_cells": counts.1,
  "pixel_width": image.width, "pixel_height": image.height,
  "photo_sha256": SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()]
print(String(data: try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys]), encoding: .utf8)!)
