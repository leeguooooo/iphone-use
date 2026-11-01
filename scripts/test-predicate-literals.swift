import Foundation
// Rust compares its real encoder against these same literals. Foundation is
// the independent oracle for their meaning; no UI or phone is involved.
struct Literal: Decodable { let input: String; let literal: String }
precondition(CommandLine.arguments.count == 2)
let samples = try JSONDecoder().decode([Literal].self, from: Data(contentsOf: URL(fileURLWithPath: CommandLine.arguments[1])))
precondition(!samples.isEmpty)
for (index, sample) in samples.enumerated() {
    let predicate = NSPredicate(format: "SELF == " + sample.literal)
    for (other, candidate) in samples.enumerated() {
        precondition(predicate.evaluate(with: candidate.input) == (index == other), "Literal changed its meaning")
    }
}
print("Foundation literal comparisons passed: \(samples.count * samples.count)")
