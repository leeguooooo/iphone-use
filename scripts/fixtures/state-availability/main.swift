import UIKit
import CryptoKit

final class StateWindow: UIWindow {
  var recordTouch: (() -> Void)?
  override func sendEvent(_ event: UIEvent) {
    let began = (event.allTouches ?? []).filter { $0.phase == .began }.count
    super.sendEvent(event)
    for _ in 0..<began { recordTouch?() }
  }
}

final class StateController: UIViewController, UITextFieldDelegate {
  let runID: String
  let controllerID = UUID().uuidString
  let enabled = UIButton(type: .system)
  let disabled = UIButton(type: .system)
  let selected = UIButton(type: .system)
  let unselected = UIButton(type: .system)
  let field = UITextField()
  let otherField = UITextField()
  var returnEvents = 0
  var sequence = 0
  var focusReceipt: [String:Any]?
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
    field.autocapitalizationType = .none
    field.delegate = self
    view.addSubview(field)
    otherField.frame = CGRect(x:20,y:435,width:330,height:50)
    otherField.borderStyle = .roundedRect
    otherField.accessibilityIdentifier = "state.other"
    otherField.text = "untouched"
    otherField.autocorrectionType = .no
    otherField.autocapitalizationType = .none
    view.addSubview(otherField)
  }
  func textFieldShouldReturn(_ textField: UITextField) -> Bool {
    returnEvents += 1
    save()
    return false
  }
  func textState(_ input: UITextField) -> [String:Any] {
    let text = input.text ?? ""
    let digest = SHA256.hash(data:Data(text.utf8)).map {String(format:"%02x",$0)}.joined()
    let selection = input.selectedTextRange
    let location = selection.map { input.offset(from:input.beginningOfDocument,to:$0.start) } ?? -1
    let length = selection.map { input.offset(from:$0.start,to:$0.end) } ?? -1
    return ["focused":input.isFirstResponder,"characters":text.unicodeScalars.count,
      "utf16_length":text.utf16.count,"sha256":digest,"selection_location":location,"selection_length":length]
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
    let before = field.isFirstResponder
    let acknowledged: Bool
    if let caret = command["caret_utf16"] as? Int {
      if focused && before && caret >= 0, let position = field.position(from:field.beginningOfDocument,offset:caret) {
        field.selectedTextRange = field.textRange(from:position,to:position)
        acknowledged = field.selectedTextRange.map { field.offset(from:field.beginningOfDocument,to:$0.start)==caret && $0.isEmpty } ?? false
      } else { acknowledged = false }
    } else {
      acknowledged = focused ? field.becomeFirstResponder() : field.resignFirstResponder()
    }
    focusReceipt = ["sequence":next,"requested":focused,"acknowledged":acknowledged,
      "before":before,"after":field.isFirstResponder]
    save()
  }
  func save() {
    guard let window = view.window else { return }
    let controls: [UIView] = [enabled,disabled,selected,unselected,field,otherField]
    let visible = controls.enumerated().compactMap { index, control -> Int? in
      !control.isHidden && control.convert(control.bounds,to:window).intersects(window.bounds) ? index : nil
    }
    let state: [String:Any] = [
      "run_id":runID,"controller_id":controllerID,"pid":Int(ProcessInfo.processInfo.processIdentifier),
      "command_sequence":sequence,"touches":touches,"tick":tick,"appeared":appeared,"return_events":returnEvents,
      "application_state":UIApplication.shared.applicationState.rawValue,"key_window":window.isKeyWindow,
      "width":window.bounds.width,"height":window.bounds.height,
      "visible_indices":visible,
      "controls":[
        "state.enabled":["enabled":enabled.isEnabled],
        "state.disabled":["enabled":disabled.isEnabled],
        "state.selected":["selected":selected.isSelected],
        "state.unselected":["selected":unselected.isSelected],
        "state.field":textState(field),
        "state.other":textState(otherField),
      ],
    ]
    var result = state
    if let focusReceipt { result["focus_command"] = focusReceipt }
    if let bytes = try? JSONSerialization.data(withJSONObject:result) {
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
