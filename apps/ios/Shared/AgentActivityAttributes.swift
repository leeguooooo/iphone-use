import ActivityKit
import Foundation

@available(iOS 16.1, *)
struct AgentActivityAttributes: ActivityAttributes, Sendable {
  struct ContentState: Codable, Hashable, Sendable {
    var current: String
    var next: String
    var steps: Int
    var terminal: Bool
    var succeeded: Bool
    var updatedAt: Date
  }
  var goal: String
  var device: String
  var route: String? = nil
}
