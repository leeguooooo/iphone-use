import ActivityKit
import SwiftUI
import WidgetKit

@main
struct AgentActivityBundle: WidgetBundle {
  @WidgetBundleBuilder var body: some Widget {
    AgentLiveActivity()
  }
}

@available(iOS 16.1, *)
struct AgentLiveActivity: Widget {
  var body: some WidgetConfiguration {
    ActivityConfiguration(for: AgentActivityAttributes.self) { context in
      VStack(alignment: .leading, spacing: 8) {
        Label(context.attributes.device, systemImage: "sparkles").font(.caption.bold())
        Text(context.attributes.goal).font(.headline).lineLimit(2).privacySensitive()
        progress(context.state, stale: isStale(context))
      }.padding(16)
        .activityBackgroundTint(Color.black.opacity(0.9))
        .activitySystemActionForegroundColor(.white)
        .widgetURL(URL(string: context.attributes.route ?? "iphoneuse://agent"))
    } dynamicIsland: { context in
      DynamicIsland {
        DynamicIslandExpandedRegion(.leading) {
          Image(systemName: "sparkles").foregroundStyle(.cyan)
        }
        DynamicIslandExpandedRegion(.trailing) {
          Text("\(context.state.steps) 步").font(.caption.monospacedDigit())
        }
        DynamicIslandExpandedRegion(.center) { Text(context.attributes.device).font(.caption) }
        DynamicIslandExpandedRegion(.bottom) { progress(context.state, stale: isStale(context)) }
      } compactLeading: {
        Image(systemName: "sparkles").foregroundStyle(.cyan)
      } compactTrailing: {
        Text(
          context.state.terminal ? (context.state.succeeded ? "✓" : "!") : "\(context.state.steps)"
        ).monospacedDigit()
      } minimal: {
        Image(
          systemName: context.state.terminal
            ? (context.state.succeeded ? "checkmark" : "exclamationmark") : "sparkles")
      }
      .widgetURL(URL(string: context.attributes.route ?? "iphoneuse://agent"))
      .keylineTint(.cyan)
    }
  }

  private func isStale(_ context: ActivityViewContext<AgentActivityAttributes>) -> Bool {
    if #available(iOS 16.2, *) { return context.isStale }
    return false
  }

  private func progress(_ state: AgentActivityAttributes.ContentState, stale: Bool) -> some View {
    VStack(alignment: .leading, spacing: 5) {
      if !state.terminal {
        Text("上次收到的状态").font(.caption2).foregroundStyle(.secondary)
      }
      Text(stale && !state.terminal ? "进度暂未更新，点开查看" : state.current).font(.headline).lineLimit(2)
      Text(state.terminal ? "下一步：\(state.next)" : "上次计划：\(state.next)")
        .font(.caption).foregroundStyle(.secondary).lineLimit(2)
      if !state.terminal {
        HStack(spacing: 4) {
          Text("最近更新")
          Text(state.updatedAt, style: .relative)
        }
        .font(.caption2).foregroundStyle(.secondary)
      }
    }.frame(maxWidth: .infinity, alignment: .leading)
  }
}

