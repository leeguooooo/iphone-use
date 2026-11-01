import UIKit

final class StateWindow: UIWindow {
  var recordTouch: (() -> Void)?
  override func sendEvent(_ event: UIEvent) {
    let began = (event.allTouches ?? []).filter { $0.phase == .began }.count
    super.sendEvent(event)
    for _ in 0..<began { recordTouch?() }
  }
}

final class StateController: UIViewController {
  let runID: String
  let controllerID = UUID().uuidString
  let enabled = UIButton(type: .system)
  let disabled = UIButton(type: .system)
  let selected = UIButton(type: .system)
  let unselected = UIButton(type: .system)
  let field = UITextField()
  var sequence = 0
  var touches = 0
  var tick = 0
  var appeared = false
  var timer: Timer?
  var documents: URL { FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0] }
  init(_ runID: String) { self.runID = runID; super.init(nibName:nil,bundle:nil) }
  required init?(coder: NSCoder) { fatalError("fixture only") }
  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .white
    for (index, pair) in [(enabled,"enabled"),(disabled,"disabled"),(selected,"selected"),(unselected,"unselected")].enumerated() {
      let (button, name) = pair
      button.frame = CGRect(x:20,y:CGFloat(80+index*65),width:330,height:50)
      button.setTitle(name,for:.normal)
      button.accessibilityIdentifier = "state." + name
      view.addSubview(button)
    }
    disabled.isEnabled = false
    selected.isSelected = true
    field.frame = CGRect(x:20,y:370,width:330,height:50)
    field.borderStyle = .roundedRect
    field.placeholder = "State focus oracle"
    field.accessibilityIdentifier = "state.field"
    field.autocorrectionType = .no
    view.addSubview(field)
  }
  override func viewDidAppear(_ animated: Bool) {
    super.viewDidAppear(animated)
    appeared = true
    timer = Timer.scheduledTimer(withTimeInterval:0.05,repeats:true) { [weak self] _ in
      guard let self else { return }
      self.tick += 1
      self.consumeCommand()
      self.save()
    }
    save()
  }
  func consumeCommand() {
    guard let bytes = try? Data(contentsOf:documents.appendingPathComponent("state-command.json")),
          let command = (try? JSONSerialization.jsonObject(with:bytes)) as? [String:Any],
          command["run_id"] as? String == runID,
          command["pid"] as? Int == Int(ProcessInfo.processInfo.processIdentifier),
          let next = command["sequence"] as? Int, next > sequence,
          let focused = command["focused"] as? Bool else { return }
    sequence = next
    if focused { _ = field.becomeFirstResponder() }
    else { _ = field.resignFirstResponder() }
    save()
  }
  func save() {
    guard let window = view.window else { return }
    let controls: [UIView] = [enabled,disabled,selected,unselected,field]
    let visible = controls.enumerated().compactMap { index, control -> Int? in
      !control.isHidden && control.convert(control.bounds,to:window).intersects(window.bounds) ? index : nil
    }
    let state: [String:Any] = [
      "run_id":runID,"controller_id":controllerID,"pid":Int(ProcessInfo.processInfo.processIdentifier),
      "command_sequence":sequence,"touches":touches,"tick":tick,"appeared":appeared,
      "application_state":UIApplication.shared.applicationState.rawValue,"key_window":window.isKeyWindow,
      "width":window.bounds.width,"height":window.bounds.height,
      "visible_indices":visible,
      "controls":[
        "state.enabled":["enabled":enabled.isEnabled],
        "state.disabled":["enabled":disabled.isEnabled],
        "state.selected":["selected":selected.isSelected],
        "state.unselected":["selected":unselected.isSelected],
        "state.field":["focused":field.isFirstResponder],
      ],
    ]
    if let bytes = try? JSONSerialization.data(withJSONObject:state) {
      try? bytes.write(to:documents.appendingPathComponent("state-oracle.json"),options:.atomic)
    }
  }
}
final class StateAppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ app: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey:Any]?) -> Bool { true }
  func application(_ app: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let configuration = UISceneConfiguration(name:"State Oracle",sessionRole:session.role)
    configuration.delegateClass = StateSceneDelegate.self
    return configuration
  }
}
final class StateSceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: StateWindow?
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let args = ProcessInfo.processInfo.arguments
    guard let index = args.firstIndex(of:"--run-id"), index+1 < args.count, UUID(uuidString:args[index+1]) != nil else { return }
    let window = StateWindow(windowScene:scene)
    let controller = StateController(args[index+1])
    window.rootViewController = controller
    window.recordTouch = { [weak controller] in
      guard let controller else { return }
      controller.touches += 1
      controller.save()
    }
    window.makeKeyAndVisible()
    self.window = window
  }
}
UIApplicationMain(CommandLine.argc,CommandLine.unsafeArgv,nil,NSStringFromClass(StateAppDelegate.self))
