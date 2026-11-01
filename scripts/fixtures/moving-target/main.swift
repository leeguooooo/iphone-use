import UIKit

final class TouchWindow: UIWindow {
  var report: ((String) -> Void)?
  override func sendEvent(_ event: UIEvent) {
    let began = (event.allTouches ?? []).filter { $0.phase == .began }
    super.sendEvent(event)
    for touch in began {
      var node = touch.view
      var recipient = "background"
      while let current = node {
        if let id = current.accessibilityIdentifier, ["race.target", "race.overlay", "race.decoy", "race.replaced"].contains(id) {
          recipient = id
          break
        }
        node = current.superview
      }
      report?(recipient)
    }
  }
}

final class SearchOverlay: UISearchTextField {
  override var canBecomeFirstResponder: Bool { false }
  override func point(inside point: CGPoint, with event: UIEvent?) -> Bool {
    bounds.insetBy(dx: -14, dy: -14).contains(point)
  }
}

final class MovingController: UIViewController {
  let runID: String
  let target = UIButton(type: .system)
  let decoy = UIButton(type: .system)
  let overlay = SearchOverlay()
  var received = ["race.target": 0, "race.overlay": 0, "race.decoy": 0, "background": 0, "race.replaced": 0]
  var activated = ["race.target": 0, "race.decoy": 0]
  var commandSequence = 0
  var moved = false
  var replaced = false
  var timer: Timer?
  var appeared = false
  var documents: URL { FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0] }
  init(runID: String) { self.runID = runID; super.init(nibName: nil, bundle: nil) }
  required init?(coder: NSCoder) { fatalError("fixture only") }

  override func viewDidLoad() {
    super.viewDidLoad()
    view.backgroundColor = .white
    for (button, id, title) in [(target, "race.target", "Moving target"), (decoy, "race.decoy", "Decoy")] {
      button.accessibilityIdentifier = id
      button.accessibilityLabel = title
      button.isAccessibilityElement = true
      button.setTitle(title, for: .normal)
      button.backgroundColor = .systemBlue.withAlphaComponent(0.15)
      button.addTarget(self, action: #selector(pressed(_:)), for: .touchUpInside)
    }
    overlay.accessibilityIdentifier = "race.overlay"
    overlay.accessibilityLabel = "Search overlay"
    overlay.placeholder = "Search overlay"
    overlay.isAccessibilityElement = true
    overlay.backgroundColor = .systemGray4
    view.addSubview(decoy)
    view.addSubview(target)
    view.addSubview(overlay)
    for name in [UIApplication.didBecomeActiveNotification, UIApplication.willResignActiveNotification, UIWindow.didBecomeKeyNotification] {
      NotificationCenter.default.addObserver(self, selector: #selector(saveState), name: name, object: nil)
    }
    timer = Timer.scheduledTimer(withTimeInterval: 0.03, repeats: true) { [weak self] _ in self?.consumeCommand() }
  }

  override func viewDidLayoutSubviews() {
    super.viewDidLayoutSubviews()
    let width = view.bounds.width
    let overlayY = view.bounds.height - 120
    let clear = CGRect(x: 16, y: 180, width: width - 32, height: 36)
    target.frame = moved ? CGRect(x: 16, y: overlayY - 4, width: width - 32, height: 36) : clear
    decoy.frame = clear
    decoy.isHidden = !moved
    overlay.frame = CGRect(x: 28, y: overlayY, width: width - 56, height: 28)
  }

  override func viewDidAppear(_ animated: Bool) {
    super.viewDidAppear(animated)
    appeared = true
    saveState()
  }

  @objc func pressed(_ sender: UIButton) {
    if let id = sender.accessibilityIdentifier { activated[id, default: 0] += 1 }
    saveState()
  }

  func recordTouch(_ recipient: String) {
    received[recipient, default: 0] += 1
    saveState()
  }

  func consumeCommand() {
    guard let bytes = try? Data(contentsOf: documents.appendingPathComponent("race-command.json")),
          let data = (try? JSONSerialization.jsonObject(with: bytes)) as? [String: Any],
          data["run_id"] as? String == runID,
          data["pid"] as? Int == Int(ProcessInfo.processInfo.processIdentifier),
          let sequence = data["sequence"] as? Int, sequence > commandSequence,
          let shouldMove = data["move"] as? Bool
    else { return }
    moved = shouldMove
    replaced = data["replace"] as? Bool ?? false
    target.accessibilityIdentifier = replaced ? "race.replaced" : "race.target"
    commandSequence = sequence
    view.setNeedsLayout()
    view.layoutIfNeeded()
    CATransaction.flush()
    UIAccessibility.post(notification: .layoutChanged, argument: nil)
    saveState()
  }

  func rect(_ element: UIView) -> [String: Double] {
    let frame = element.convert(element.bounds, to: view.window)
    return ["x": frame.minX, "y": frame.minY, "width": frame.width, "height": frame.height].mapValues(Double.init)
  }

  @objc func saveState() {
    guard isViewLoaded, let window = view.window else { return }
    let state: [String: Any] = [
      "run_id": runID, "pid": Int(ProcessInfo.processInfo.processIdentifier),
      "application_state": UIApplication.shared.applicationState.rawValue,
      "key_window": window.isKeyWindow, "appeared": appeared,
      "width": window.bounds.width, "height": window.bounds.height,
      "visible_indices": [0, 1], "command_sequence": commandSequence, "moved": moved, "replaced": replaced,
      "received": received, "activated": activated,
      "target_rect": rect(target), "overlay_rect": rect(overlay), "decoy_rect": rect(decoy),
    ]
    guard let bytes = try? JSONSerialization.data(withJSONObject: state, options: [.sortedKeys]) else { return }
    try? bytes.write(to: documents.appendingPathComponent("race-state.json"), options: [.atomic])
  }
}

final class MovingAppDelegate: UIResponder, UIApplicationDelegate {
  func application(_ app: UIApplication, didFinishLaunchingWithOptions options: [UIApplication.LaunchOptionsKey: Any]?) -> Bool { true }
  func application(_ app: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let configuration = UISceneConfiguration(name: "Moving Target Oracle", sessionRole: session.role)
    configuration.delegateClass = MovingSceneDelegate.self
    return configuration
  }
}

final class MovingSceneDelegate: UIResponder, UIWindowSceneDelegate {
  var window: TouchWindow?
  func scene(_ scene: UIScene, willConnectTo session: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let args = ProcessInfo.processInfo.arguments
    guard let index = args.firstIndex(of: "--run-id"), index + 1 < args.count,
          UUID(uuidString: args[index + 1]) != nil else { return }
    let window = TouchWindow(windowScene: scene)
    let controller = MovingController(runID: args[index + 1])
    window.rootViewController = controller
    window.report = { [weak controller] recipient in controller?.recordTouch(recipient) }
    window.makeKeyAndVisible()
    self.window = window
  }
}

UIApplicationMain(CommandLine.argc, CommandLine.unsafeArgv, nil, NSStringFromClass(MovingAppDelegate.self))
