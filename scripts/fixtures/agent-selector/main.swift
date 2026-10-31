import UIKit
import CryptoKit

// An operator supplies a private case file; accessibility IDs and row order
// carry no role names. The oracle records actual UIKit properties after setup.
final class AgentSelectorController: UIViewController {
  let runID: String
  let caseDigest: String
  let definitions: [[String: Any]]
  var rows = [[String: Any]]()
  var inputEvents = 0
  var buttonActivations = 0
  var foregroundLosses = 0
  init(runID: String, digest: String, rows: [[String: Any]]) {
    self.runID = runID; caseDigest = digest; definitions = rows
    super.init(nibName: nil, bundle: nil)
  }
  required init?(coder: NSCoder) { fatalError("fixture only") }
  override func viewDidLoad() {
    super.viewDidLoad(); view.backgroundColor = .white
    for (index, definition) in definitions.enumerated() {
      let identifier = definition["identifier"] as! String
      let label = definition["label"] as! String
      let kind = definition["kind"] as! String
      let enabled = definition["enabled"] as! Bool
      let element: UIView
      if kind == "Button" {
        let control = UIButton(type: .system)
        control.setTitle(label, for: .normal); control.isEnabled = enabled
        control.addTarget(self, action: #selector(tapped), for: .touchUpInside)
        element = control
      } else {
        let text = UILabel(); text.text = label; text.backgroundColor = .lightGray
        text.accessibilityTraits = .staticText; element = text
      }
      element.frame = CGRect(x: 20, y: CGFloat(80 + index * 75), width: 330, height: 60)
      element.isAccessibilityElement = true
      element.accessibilityIdentifier = identifier; element.accessibilityLabel = label
      element.accessibilityValue = definition["value"] as? String
      view.addSubview(element)
      rows.append(["identifier": element.accessibilityIdentifier!, "label": element.accessibilityLabel!,
                   "value": element.accessibilityValue.map { $0 as Any } ?? NSNull(),
                   "enabled": (element as? UIControl)?.isEnabled ?? true, "kind": kind])
    }
    for name in [UIApplication.didBecomeActiveNotification, UIApplication.didEnterBackgroundNotification,
                 UIWindow.didBecomeKeyNotification] {
      NotificationCenter.default.addObserver(self, selector: #selector(saveState), name: name, object: nil)
    }
    NotificationCenter.default.addObserver(self, selector:#selector(resigned), name:UIApplication.willResignActiveNotification, object:nil)
    write(["run_id":runID, "case_sha256":caseDigest, "rows":rows], name:"agent-selector-oracle.json")
  }
  override func viewDidAppear(_ animated: Bool) { super.viewDidAppear(animated); saveState() }
  @objc func tapped() { buttonActivations += 1; saveState() }
  @objc func resigned() { foregroundLosses += 1; saveState() }
  func receivedInput() { inputEvents += 1; saveState() }
  func write(_ object: [String:Any], name: String) {
    let path = FileManager.default.urls(for:.documentDirectory,in:.userDomainMask)[0].appendingPathComponent(name)
    do {
      let data = try JSONSerialization.data(withJSONObject:object)
      try data.write(to:path, options:.atomic)
    } catch {
      // A stale clean state must never outlive an unrecorded input. The
      // operator also checks the live foreground PID before accepting proof.
      fatalError("fixture oracle persistence failed")
    }
  }
  @objc func saveState() {
    write(["run_id":runID, "case_sha256":caseDigest, "pid":ProcessInfo.processInfo.processIdentifier,
           "input_events":inputEvents, "button_activations":buttonActivations,
           "foreground_losses":foregroundLosses, "application_state":UIApplication.shared.applicationState.rawValue,
           "key_window":view.window?.isKeyWindow ?? false], name:"agent-selector-state.json")
  }
}
// Counts events delivered to this fixture; it is not a system-wide input monitor.
final class AgentSelectorWindow: UIWindow {
  override func sendEvent(_ event:UIEvent) {
    if (event.type == .touches && event.allTouches?.isEmpty == false) || event.type == .presses {
      (rootViewController as? AgentSelectorController)?.receivedInput()
    }
    super.sendEvent(event)
  }
}
final class AgentSelectorAppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ app:UIApplication, didFinishLaunchingWithOptions options:[UIApplication.LaunchOptionsKey:Any]?) -> Bool { true }
  func application(_ app:UIApplication, configurationForConnecting session:UISceneSession, options:UIScene.ConnectionOptions) -> UISceneConfiguration {
    let config = UISceneConfiguration(name:"Records",sessionRole:session.role)
    config.delegateClass = AgentSelectorSceneDelegate.self; return config
  }
}
final class AgentSelectorSceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  func scene(_ scene:UIScene, willConnectTo session:UISceneSession, options:UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let path = FileManager.default.urls(for:.documentDirectory,in:.userDomainMask)[0].appendingPathComponent("agent-selector-case.json")
    guard let data = try? Data(contentsOf:path),
          let value = try? JSONSerialization.jsonObject(with:data) as? [String:Any],
          let runID = value["run_id"] as? String, UUID(uuidString:runID) != nil,
          let rows = value["rows"] as? [[String:Any]], rows.count == 8,
          Set(rows.compactMap { $0["identifier"] as? String }).count == 8,
          rows.allSatisfy({ row in
            row["identifier"] is String && row["label"] as? String == "Record" &&
            ["Button","StaticText"].contains(row["kind"] as? String ?? "") && row["enabled"] is Bool &&
            (row["value"] is String || row["value"] is NSNull)
          }) else { return }
    let digest = SHA256.hash(data:data).map { String(format:"%02x",$0) }.joined()
    let window = AgentSelectorWindow(windowScene:scene)
    window.rootViewController = AgentSelectorController(runID:runID,digest:digest,rows:rows)
    window.makeKeyAndVisible(); self.window = window
  }
}
UIApplicationMain(CommandLine.argc,CommandLine.unsafeArgv,nil,NSStringFromClass(AgentSelectorAppDelegate.self))
