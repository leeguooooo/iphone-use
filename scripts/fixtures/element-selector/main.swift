import UIKit

final class SelectorController: UIViewController {
  let runID: String
  var rows = [[String: Any]]()
  var touches = 0
  init(runID: String) { self.runID = runID; super.init(nibName: nil, bundle: nil) }
  required init?(coder: NSCoder) { fatalError("fixture only") }
  override func viewDidLoad() {
    super.viewDidLoad(); view.backgroundColor = .white
    let definitions: [(String, String, String?, Bool, Bool)] = [
      ("selector.primary", "Save", "stable", true, true),
      ("selector.secondary", "Save", nil, true, true),
      ("selector.disabled", "Save", nil, false, true),
      ("selector.unicode", "保存\n'O\\K'🙂", nil, true, true),
      ("selector.empty", "Explicit empty", "", true, false),
      ("selector.missing-value", "Missing value", nil, true, false)
    ]
    for (index, definition) in definitions.enumerated() {
      let (identifier, label, value, enabled, button) = definition
      let element: UIView
      if button {
        let control = UIButton(type: .system)
        control.setTitle(label, for: .normal); control.titleLabel?.numberOfLines = 0
        control.isEnabled = enabled; control.addTarget(self, action: #selector(tapped), for: .touchUpInside)
        element = control
      } else {
        let text = UILabel(); text.text = label; text.backgroundColor = .lightGray
        element = text
      }
      element.frame = CGRect(x: 20, y: CGFloat(80 + index * 90), width: 330, height: 70)
      element.isAccessibilityElement = true
      element.accessibilityIdentifier = identifier; element.accessibilityLabel = label
      element.accessibilityValue = value
      if !button { element.accessibilityTraits = .staticText }
      view.addSubview(element)
      rows.append(["identifier": identifier, "label": label,
                   "value": element.accessibilityValue.map { $0 as Any } ?? NSNull(),
                   "enabled": enabled, "kind": button ? "Button" : "StaticText"])
    }
    NotificationCenter.default.addObserver(self, selector: #selector(activated), name: UIApplication.didBecomeActiveNotification, object: nil)
    NotificationCenter.default.addObserver(self, selector: #selector(activated), name: UIWindow.didBecomeKeyNotification, object: nil)
    write(["run_id":runID, "rows":rows], name:"selector-oracle.json")
  }
  override func viewDidAppear(_ animated: Bool) { super.viewDidAppear(animated); saveState() }
  @objc func activated() { saveState() }
  @objc func tapped() { touches += 1; saveState() }
  func write(_ object: [String:Any], name: String) {
    let path = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].appendingPathComponent(name)
    if let data = try? JSONSerialization.data(withJSONObject:object) { try? data.write(to:path, options:.atomic) }
  }
  func saveState() {
    write(["run_id":runID, "pid":ProcessInfo.processInfo.processIdentifier, "touches":touches,
           "application_state":UIApplication.shared.applicationState.rawValue,
           "key_window":view.window?.isKeyWindow ?? false], name:"selector-state.json")
  }
}
final class SelectorAppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ app: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey:Any]?) -> Bool { true }
  func application(_ app: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let config = UISceneConfiguration(name:"Selector Oracle", sessionRole:session.role)
    config.delegateClass = SelectorSceneDelegate.self; return config
  }
}
final class SelectorSceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let args = ProcessInfo.processInfo.arguments
    guard let index = args.firstIndex(of:"--run-id"), index + 1 < args.count, UUID(uuidString:args[index+1]) != nil else { return }
    let window = UIWindow(windowScene:scene)
    window.rootViewController = SelectorController(runID:args[index+1]); window.makeKeyAndVisible(); self.window = window
  }
}
UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil, NSStringFromClass(SelectorAppDelegate.self))
