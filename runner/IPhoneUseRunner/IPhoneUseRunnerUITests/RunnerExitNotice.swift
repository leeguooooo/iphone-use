// The on-phone way out: a notification with an End button while the runner serves.
//
// iOS gives no app a button over other apps, and its own way out of UI automation (holding both
// volume keys) also changes the volume. A notification stays on the Lock Screen and in
// Notification Center (delivered passively, without a banner over the app being driven), so the
// person holding the phone can end the session from there: the
// button ends the serve loop like POST /shutdown, the runner exits, and the Mac supervisor
// parks instead of starting it again.
//
// IPU_RUNNER_EXIT_NOTICE=0 disables it; =1 explicitly requests notification permission.
// Otherwise only an existing authorization is used, without covering the controlled app.

import Foundation
import UserNotifications

final class RunnerExitNotice: NSObject, UNUserNotificationCenterDelegate {
  static let categoryID = "ipu.session"
  static let endActionID = "ipu.session.end"
  private let requestID = "ipu.session.\(UUID().uuidString)"
  private let queue = DispatchQueue(label: "ipu.exit-notice")
  private var active = true

  private let onEnd: () -> Void

  init(onEnd: @escaping () -> Void) {
    self.onEnd = onEnd
  }

  private static var chinese: Bool {
    Locale.preferredLanguages.first?.hasPrefix("zh") ?? false
  }

  func post(requestPermission: Bool) {
    #if targetEnvironment(simulator)
    return
    #else
    let center = UNUserNotificationCenter.current()
    center.delegate = self
    let end = UNNotificationAction(
      identifier: Self.endActionID,
      title: Self.chinese ? "结束控制" : "End control",
      options: [.destructive])
    center.setNotificationCategories([
      UNNotificationCategory(
        identifier: Self.categoryID, actions: [end], intentIdentifiers: [], options: [])
    ])
    let authorized: (Bool) -> Void = { [self] granted in
      queue.async { [self] in
        guard active, granted else { return }
        let content = UNMutableNotificationContent()
        content.title = Self.chinese ? "iPhone Use 正在控制这台手机" : "iPhone Use is controlling this iPhone"
        content.body = Self.chinese
          ? "长按这条通知，点「结束控制」把手机还给你；电脑不会自动重连。"
          : "Touch and hold this notification, then tap End control to take the phone back. The Mac will not reconnect on its own."
        content.categoryIdentifier = Self.categoryID
        content.threadIdentifier = Self.categoryID
        // Into Notification Center and the Lock Screen only: a banner would sit over the app being
        // driven, where one stray touch ends the session (#413).
        if #available(iOS 15.0, *) { content.interruptionLevel = .passive }
        let request = UNNotificationRequest(identifier: requestID, content: content, trigger: nil)
        center.add(request) { [self] error in
          queue.async { [self] in
            // add is asynchronous: teardown may have happened while it was in flight.
            if !active { removeRequests(center) }
            if let error { NSLog("ipu-runner: exit notice not posted: %@", error.localizedDescription) }
          }
        }
      }
    }
    if requestPermission {
      center.requestAuthorization(options: [.alert]) { granted, _ in authorized(granted) }
    } else {
      center.getNotificationSettings { settings in
        authorized(settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional)
      }
    }
    #endif
  }

  private func removeRequests(_ center: UNUserNotificationCenter) {
    center.removeDeliveredNotifications(withIdentifiers: [requestID])
    center.removePendingNotificationRequests(withIdentifiers: [requestID])
  }

  func remove() {
    queue.sync {
      active = false
      removeRequests(UNUserNotificationCenter.current())
    }
  }

  func userNotificationCenter(
    _ center: UNUserNotificationCenter, willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    // List only, never a banner over the controlled app (#413).
    completionHandler([.list])
  }

  /// Only the explicit End action ends this session. Body taps and stale notices do not.
  func userNotificationCenter(
    _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
    withCompletionHandler completionHandler: @escaping () -> Void
  ) {
    let action = response.actionIdentifier
    queue.sync {
      if active && response.notification.request.identifier == requestID && action == Self.endActionID {
        active = false
        removeRequests(center)
        NSLog("ipu-runner: session ended from the notification")
        onEnd()
      }
    }
    completionHandler()
  }
}
