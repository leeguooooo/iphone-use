// iphone-use-ocr: recognize the text in a screenshot with Apple's Vision framework.
//
// The daemon embeds this source and compiles it on first use
// (`xcrun swiftc -O ocr.swift -o <state dir>/bin/iphone-use-ocr`), for
// screens whose accessibility tree is empty or useless (games, canvases,
// Flutter/Unity apps).
//
// Usage: iphone-use-ocr <image.png>
// Prints a JSON array on stdout:
//   [{"text": "...", "confidence": 0.98, "rect": [x, y, w, h]}, ...]
// with rects in image pixels, top-left origin. Exits non-zero with a message
// on stderr when the image cannot be read or recognition fails.

import Foundation
import ImageIO
import Vision

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("iphone-use-ocr: \(message)\n".utf8))
    exit(1)
}

let arguments = CommandLine.arguments
guard arguments.count == 2 else {
    fail("usage: iphone-use-ocr <image.png>")
}

let url = URL(fileURLWithPath: arguments[1])
guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
      let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
else {
    fail("cannot read an image from \(arguments[1])")
}
let width = Double(image.width)
let height = Double(image.height)

let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.usesLanguageCorrection = true
// Ask only for languages this macOS can recognize; an unsupported one fails
// the whole request.
let wanted = ["zh-Hans", "zh-Hant", "en-US"]
if let supported = try? request.supportedRecognitionLanguages() {
    let languages = wanted.filter { supported.contains($0) }
    if !languages.isEmpty {
        request.recognitionLanguages = languages
    }
}
request.automaticallyDetectsLanguage = true

let handler = VNImageRequestHandler(cgImage: image, options: [:])
do {
    try handler.perform([request])
} catch {
    fail("text recognition failed: \(error.localizedDescription)")
}

var rows: [[String: Any]] = []
for observation in (request.results ?? []).compactMap({ $0 as? VNRecognizedTextObservation }) {
    guard let candidate = observation.topCandidates(1).first else { continue }
    // Vision boxes are normalized with a bottom-left origin.
    let box = observation.boundingBox
    let x = Double(box.minX) * width
    let y = (1.0 - Double(box.maxY)) * height
    let w = Double(box.width) * width
    let h = Double(box.height) * height
    rows.append([
        "text": candidate.string,
        "confidence": Double(candidate.confidence),
        "rect": [x, y, w, h],
    ])
}

do {
    let data = try JSONSerialization.data(withJSONObject: rows, options: [])
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
} catch {
    fail("cannot encode the result: \(error.localizedDescription)")
}
