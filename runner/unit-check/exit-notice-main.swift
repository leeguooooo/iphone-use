import Foundation

func check(_ condition: @autoclosure () -> Bool, _ message: String) {
  if !condition() { fatalError(message) }
}
func wait(_ condition: () -> Bool) {
  let deadline = Date().addingTimeInterval(2)
  while !condition(), Date() < deadline { Thread.sleep(forTimeInterval: 0.005) }
  check(condition(), "asynchronous notice callback timed out")
}
let center = UNUserNotificationCenter.current()
var ended = 0
let notice = RunnerExitNotice { ended += 1 }
notice.post(requestPermission: false)
check(center.authorize == nil, "default startup must not request permission")
center.settings!(Settings(authorizationStatus: .authorized))
wait { center.requests().count == 1 }
let request = center.requests()[0].0
check(request.content.interruptionLevel == .passive, "the notice must not interrupt the controlled app (#413)")
var presented: UNNotificationPresentationOptions?
notice.userNotificationCenter(center, willPresent: UNNotification(request: request)) { presented = $0 }
check(presented == [.list], "the notice must never show as a banner (#413)")
func respond(_ target: RunnerExitNotice, _ request: UNNotificationRequest, _ action: String) {
  var completed = false
  target.userNotificationCenter(center, didReceive: UNNotificationResponse(actionIdentifier: action,
    notification: UNNotification(request: request))) { completed = true }
  check(completed, "notification completion must always run")
}
respond(notice, request, UNNotificationDefaultActionIdentifier)
check(ended == 0, "body tap must not end control")
respond(notice, request, RunnerExitNotice.endActionID)
respond(notice, request, RunnerExitNotice.endActionID)
check(ended == 1, "explicit End must end the session exactly once")
notice.remove()

center.reset()
let delayed = RunnerExitNotice { ended += 1 }
delayed.post(requestPermission: true)
check(center.authorize != nil && center.settings == nil, "only explicit setup may request permission")
delayed.remove()
center.authorize!(true, nil)
// queue.sync in remove drains the authorization callback scheduled immediately above.
delayed.remove()
check(center.requests().isEmpty, "authorization after teardown must not post")

center.reset()
let inflight = RunnerExitNotice { ended += 1 }
inflight.post(requestPermission: false)
center.settings!(Settings(authorizationStatus: .authorized))
wait { center.requests().count == 1 }
let old = center.requests()[0]
inflight.remove()
old.1(nil)
inflight.remove()
check(center.wasRemoved(old.0.identifier), "in-flight add completion must remove the old notice")
let next = RunnerExitNotice { ended += 1 }
respond(next, old.0, RunnerExitNotice.endActionID)
check(ended == 1, "old notification must not end a later session")
next.remove()

center.reset()
let denied = RunnerExitNotice { ended += 1 }
denied.post(requestPermission: false)
center.settings!(Settings(authorizationStatus: .denied))
denied.remove()
check(center.requests().isEmpty, "denied permission must not post or prompt")
print("exit notice: permission, body tap, explicit action, teardown, stale session and denial passed")
