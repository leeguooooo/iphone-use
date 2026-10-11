@preconcurrency import ActivityKit
@preconcurrency import BackgroundTasks
import Foundation
import UIKit

/// One visible task, matching the app's followed task. The system's continued
/// processing activity keeps network polling alive on iOS 26+. Earlier systems
/// use ActivityKit and mark content stale when the app stops getting updates.
@MainActor
final class AgentLiveStatus {
  static let shared = AgentLiveStatus()
  private let prefix = "com.leeguoo.iphone-use.remote.agent."
  private var systemTask: BGTask?
  private var requestID: String?
  private var legacyActivity: AnyObject?
  private var legacyKey: String?
  private var legacyUpdates: Task<Void, Never>?
  private var cancel: (@MainActor () async -> Void)?
  private var latest = AgentProgress(
    task: AgentTaskSnapshot(id: "pending", goal: "", state: .running))
  private var loggedStep = -1
  private init() {}

  private var useContinuedProcessing: Bool {
    #if DEBUG
      return !UserDefaults.standard.bool(forKey: "legacyActivity")
    #else
      return true
    #endif
  }

  func begin(goal: String, device: String, route: String = "iphoneuse://agent", onCancel: @escaping @MainActor () async -> Void) {
    finish(success: false, dismissImmediately: true)
    cancel = onCancel
    latest = AgentProgress(task: AgentTaskSnapshot(id: "pending", goal: goal, state: .running))
    if #available(iOS 26, *), useContinuedProcessing {
      let id = prefix + UUID().uuidString
      let registration = BGTaskScheduler.shared.register(forTaskWithIdentifier: id, using: .main) {
        [weak self] task in
        MainActor.assumeIsolated { self?.attach(task) }
      }
      #if DEBUG
        print("[AgentLiveStatus] continued registration=\(registration)")
      #endif
      if registration {
        requestID = id
        let request = BGContinuedProcessingTaskRequest(
          identifier: id, title: "AI · " + latest.current,
          subtitle: "下一步：" + latest.next)
        request.strategy = .fail
        do {
          try BGTaskScheduler.shared.submit(request)
          #if DEBUG
            print("[AgentLiveStatus] continued request accepted")
          #endif
          return
        } catch {
          requestID = nil
          #if DEBUG
            print("[AgentLiveStatus] continued request failed: \(error.localizedDescription)")
          #endif
        }
      }
    }
    if #available(iOS 16.1, *) {
      #if DEBUG
        print(
          "[AgentLiveStatus] activities enabled=\(ActivityAuthorizationInfo().areActivitiesEnabled)"
        )
      #endif
      guard ActivityAuthorizationInfo().areActivitiesEnabled else { return }
      let attributes = AgentActivityAttributes(
        goal: String(goal.prefix(80)), device: String(device.prefix(48)), route: route)
      do {
        if #available(iOS 16.2, *) {
          legacyActivity = try Activity.request(
            attributes: attributes,
            content: ActivityContent(
              state: content(), staleDate: Date().addingTimeInterval(20), relevanceScore: 1),
            pushType: nil)
        } else {
          legacyActivity = try Activity.request(
            attributes: attributes, contentState: content(), pushType: nil)
        }
        #if DEBUG
          print("[AgentLiveStatus] activity created")
        #endif
      } catch {
        legacyActivity = nil
        #if DEBUG
          print("[AgentLiveStatus] activity failed: \(error)")
        #endif
      }
    }
  }

  private func attach(_ task: BGTask) {
    guard task.identifier == requestID else {
      task.setTaskCompleted(success: false)
      return
    }
    systemTask = task
    #if DEBUG
      print("[AgentLiveStatus] continued task attached")
    #endif
    let id = task.identifier
    task.expirationHandler = { [weak self] in
      Task { @MainActor in
        guard let self, self.requestID == id else { return }
        #if DEBUG
          print("[AgentLiveStatus] system expiration")
        #endif
        let cancel = self.cancel
        await cancel?()
        guard self.requestID == id else { return }
        self.finish(success: false)
      }
    }
    refreshSystem()
  }

  func bind(taskID: String, deviceID: UUID) {
    legacyKey = "agent.activity.\(deviceID.uuidString)|\(taskID)"
    if #available(iOS 16.1, *), let activity = legacyActivity as? Activity<AgentActivityAttributes>
    {
      UserDefaults.standard.set(activity.id, forKey: legacyKey!)
    }
  }

  func recover(
    task: AgentTaskSnapshot, device: String, deviceID: UUID,
    onCancel: @escaping @MainActor () async -> Void
  ) {
    let key = "agent.activity.\(deviceID.uuidString)|\(task.id)"
    if legacyKey == key && (systemTask != nil || requestID != nil) {
      cancel = onCancel
      update(AgentProgress(task: task))
      if task.state.isTerminal { finish(success: task.state == .done) }
      return
    }
    let savedID = UserDefaults.standard.string(forKey: key)
    if #available(iOS 16.1, *),
      let activity = Activity<AgentActivityAttributes>.activities.first(where: { $0.id == savedID })
    {
      legacyActivity = activity
      legacyKey = key
      cancel = onCancel
    } else if !task.state.isTerminal {
      begin(goal: task.goal, device: device, onCancel: onCancel)
      bind(taskID: task.id, deviceID: deviceID)
    }
    update(AgentProgress(task: task))
    if task.state.isTerminal { finish(success: task.state == .done) }
  }

  func update(_ progress: AgentProgress) {
    latest = progress
    #if DEBUG
      if progress.steps != loggedStep {
        loggedStep = progress.steps
        print(
          "[AgentLiveStatus] steps=\(progress.steps) app_state=\(UIApplication.shared.applicationState.rawValue)"
        )
      }
    #endif
    refreshSystem()
    if #available(iOS 16.1, *), let activity = legacyActivity as? Activity<AgentActivityAttributes>
    {
      let state = content()
      let previous = legacyUpdates
      legacyUpdates = Task {
        await previous?.value
        if #available(iOS 16.2, *) {
          await activity.update(
            ActivityContent(
              state: state, staleDate: Date().addingTimeInterval(20), relevanceScore: 1))
        } else {
          await activity.update(using: state)
        }
      }
    }
  }

  private func refreshSystem() {
    if #available(iOS 26, *), let task = systemTask as? BGContinuedProcessingTask {
      task.updateTitle(
        "AI · " + String(latest.current.prefix(50)),
        subtitle: "\(latest.steps) 步 · 下一步：" + String(latest.next.prefix(42)))
      // Completed steps plus the pending result: the discovered total
      // grows with real work. The text reports steps, never a guessed ETA.
      task.progress.totalUnitCount = Int64(latest.steps + 1)
      task.progress.completedUnitCount = Int64(latest.steps)
      task.progress.localizedDescription = String(localized: "已执行 \(latest.steps) 步")
    }
  }

  func finish(success: Bool, dismissImmediately: Bool = false) {
    if !latest.terminal {
      latest.current = String(localized: "进度跟随暂停")
      latest.next = String(localized: "点开任务刷新")
      latest.terminal = true
      latest.succeeded = false
    }
    if #available(iOS 26, *), let task = systemTask as? BGContinuedProcessingTask {
      refreshSystem()
      if success {
        task.progress.totalUnitCount = Int64(max(1, latest.steps + 1))
        task.progress.completedUnitCount = task.progress.totalUnitCount
      }
      task.setTaskCompleted(success: success)
    }
    systemTask = nil
    if let id = requestID { BGTaskScheduler.shared.cancel(taskRequestWithIdentifier: id) }
    requestID = nil
    cancel = nil
    if #available(iOS 16.1, *), let activity = legacyActivity as? Activity<AgentActivityAttributes>
    {
      let state = content()
      let previous = legacyUpdates
      legacyUpdates = Task {
        await previous?.value
        if #available(iOS 16.2, *) {
          await activity.end(
            ActivityContent(state: state, staleDate: nil),
            dismissalPolicy: dismissImmediately ? .immediate : .after(Date().addingTimeInterval(120)))
        } else {
          await activity.end(using: state, dismissalPolicy: dismissImmediately ? .immediate : .after(Date().addingTimeInterval(120)))
        }
      }
    }
    legacyActivity = nil
    if let key = legacyKey { UserDefaults.standard.removeObject(forKey: key) }
    legacyKey = nil
  }

  @available(iOS 16.1, *)
  private func content() -> AgentActivityAttributes.ContentState {
    .init(
      current: latest.current, next: latest.next, steps: latest.steps,
      terminal: latest.terminal, succeeded: latest.succeeded, updatedAt: .now)
  }
}
