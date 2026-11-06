import Foundation

enum UNAuthorizationOption { case alert }
enum UNActionOption { case destructive }
enum UNPresentationOption { case banner, list }
typealias UNNotificationPresentationOptions = Set<UNPresentationOption>
enum Authorization { case authorized, provisional, denied, notDetermined }
struct Settings { let authorizationStatus: Authorization }
protocol UNUserNotificationCenterDelegate: AnyObject {}
let UNNotificationDefaultActionIdentifier = "default"
struct UNNotificationAction {
  let identifier: String
  let title: String
  let options: Set<UNActionOption>
}
struct UNNotificationCategory {
  let identifier: String
  let actions: [UNNotificationAction]
  let intentIdentifiers: [String]
  let options: Set<String>
}
enum UNNotificationInterruptionLevel { case passive, active, timeSensitive, critical }
class UNMutableNotificationContent {
  var title = "", body = "", categoryIdentifier = "", threadIdentifier = ""
  var interruptionLevel = UNNotificationInterruptionLevel.active
}
struct UNNotificationRequest {
  let identifier: String
  let content: UNMutableNotificationContent
  let trigger: String?
}
struct UNNotification { let request: UNNotificationRequest }
struct UNNotificationResponse {
  let actionIdentifier: String
  let notification: UNNotification
}
final class UNUserNotificationCenter {
  static let shared = UNUserNotificationCenter()
  static func current() -> UNUserNotificationCenter { shared }
  weak var delegate: UNUserNotificationCenterDelegate?
  var authorize: ((Bool, Error?) -> Void)?
  var settings: ((Settings) -> Void)?
  private let lock = NSLock()
  private var pending: [(UNNotificationRequest, (Error?) -> Void)] = []
  private var removed: Set<String> = []
  func setNotificationCategories(_ categories: Set<UNNotificationCategory>) {}
  func requestAuthorization(options: Set<UNAuthorizationOption>, completionHandler: @escaping (Bool, Error?) -> Void) {
    authorize = completionHandler
  }
  func getNotificationSettings(completionHandler: @escaping (Settings) -> Void) { settings = completionHandler }
  func add(_ request: UNNotificationRequest, withCompletionHandler completionHandler: @escaping (Error?) -> Void) {
    lock.lock(); defer { lock.unlock() }
    pending.append((request, completionHandler))
  }
  func removeDeliveredNotifications(withIdentifiers ids: [String]) {
    lock.lock(); defer { lock.unlock() }; removed.formUnion(ids)
  }
  func removePendingNotificationRequests(withIdentifiers ids: [String]) {
    lock.lock(); defer { lock.unlock() }; removed.formUnion(ids)
  }
  func requests() -> [(UNNotificationRequest, (Error?) -> Void)] {
    lock.lock(); defer { lock.unlock() }; return pending
  }
  func wasRemoved(_ id: String) -> Bool {
    lock.lock(); defer { lock.unlock() }; return removed.contains(id)
  }
  func reset() { authorize = nil; settings = nil; lock.lock(); pending = []; removed = []; lock.unlock() }
}
extension UNNotificationCategory: Hashable {
  static func == (lhs: Self, rhs: Self) -> Bool { lhs.identifier == rhs.identifier }
  func hash(into hasher: inout Hasher) { hasher.combine(identifier) }
}
