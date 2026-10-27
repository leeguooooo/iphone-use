import Foundation
import Vision
let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.recognitionLanguages = ["en-US"]
request.usesLanguageCorrection = false
try VNImageRequestHandler(url: URL(fileURLWithPath: CommandLine.arguments[1]), options: [:]).perform([request])
let lines = (request.results ?? []).compactMap { result -> [String: Any]? in
  guard let text = result.topCandidates(1).first else { return nil }
  return ["text": text.string, "confidence": text.confidence, "x": result.boundingBox.minX, "y": result.boundingBox.minY]
}
let data = try JSONSerialization.data(withJSONObject: lines, options: [.sortedKeys])
print(String(data: data, encoding: .utf8)!)
