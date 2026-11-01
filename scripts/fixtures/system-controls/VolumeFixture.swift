import UIKit
import AVFoundation

@main final class VolumeFixture: UIResponder, UIApplicationDelegate {
  func application(_ application: UIApplication, configurationForConnecting session: UISceneSession, options: UIScene.ConnectionOptions) -> UISceneConfiguration {
    let c = UISceneConfiguration(name: "Default", sessionRole: session.role); c.delegateClass = VolumeScene.self; return c
  }
}
@objc(VolumeScene) final class VolumeScene: UIResponder, UIWindowSceneDelegate {
  var window: UIWindow?
  let runID = UUID().uuidString
  let session = AVAudioSession.sharedInstance()
  var player: AVAudioPlayer?
  var observation: NSKeyValueObservation?
  var baseline: Float?
  var changes: [Float] = []
  var state = "ready"
  let status = UILabel(), initial = UILabel(), current = UILabel(), count = UILabel()
  func scene(_ scene: UIScene, willConnectTo: UISceneSession, options: UIScene.ConnectionOptions) {
    guard let scene = scene as? UIWindowScene else { return }
    let c = UIViewController(); c.view.backgroundColor = .white
    let w = UIWindow(windowScene: scene); w.rootViewController = c; window = w; w.makeKeyAndVisible()
    let labels = [status, initial, current, count]
    for (i,l) in labels.enumerated() {
      l.frame = CGRect(x: 20, y: 130 + i * 65, width: 350, height: 60); l.textColor = .black; l.font = .monospacedSystemFont(ofSize: 19, weight: .medium); c.view.addSubview(l)
    }
    for (title, action, y) in [("开始无声音量验证", #selector(start), 430), ("结束无声音量验证", #selector(stopAudio), 510)] {
      let b = UIButton(type: .system); b.frame = CGRect(x: 20, y: y, width: 350, height: 60); b.setTitle(title, for: .normal); b.addTarget(self, action: action, for: .touchUpInside); c.view.addSubview(b)
    }
    observation = session.observe(\.outputVolume, options: [.new]) { [weak self] _, change in
      DispatchQueue.main.async {
        guard let self, self.state == "active", let value = change.newValue else { return }
        self.changes.append(value); self.refresh()
      }
    }
    refresh()
  }
  @objc func start() {
    guard state == "ready" else { return }
    do {
      try session.setCategory(.playback, options: [.mixWithOthers]); try session.setActive(true)
      var wav = Data()
      func word(_ value: UInt32, _ bytes: Int) { for n in 0..<bytes { wav.append(UInt8(truncatingIfNeeded: value >> (n * 8))) } }
      wav.append(contentsOf: "RIFF".utf8); word(1636,4); wav.append(contentsOf: "WAVEfmt ".utf8); word(16,4); word(1,2); word(1,2); word(8000,4); word(16000,4); word(2,2); word(16,2); wav.append(contentsOf: "data".utf8); word(1600,4); wav.append(Data(repeating:0,count:1600))
      let p = try AVAudioPlayer(data: wav); p.volume = 0; p.numberOfLoops = -1; p.prepareToPlay()
      guard p.play() else { throw NSError(domain:"VolumeFixture",code:1) }
      player = p; baseline = session.outputVolume; changes = []; state = "active"; refresh()
      DispatchQueue.main.asyncAfter(deadline: .now() + 90) { [weak self] in
        if self?.state == "active" { self?.stopAudio() }
      }
    } catch { state = "error"; refresh() }
  }
  @objc func stopAudio() {
    player?.stop(); player = nil
    do { try session.setActive(false, options: [.notifyOthersOnDeactivation]); state = "ended" }
    catch { state = "stop_error" }
    refresh()
  }
  func refresh() {
    status.text = "state=\(state)"; initial.text = "baseline=\(baseline.map(String.init(describing:)) ?? "unset")"
    current.text = "current=\(session.outputVolume)"; count.text = "changes=\(changes.count)"
    let d: [String:Any] = ["run_id":runID,"state":state,"baseline":baseline as Any? ?? NSNull(),"current":session.outputVolume,"changes":changes,"silent_player":player != nil,"time":Date().timeIntervalSince1970]
    if let data = try? JSONSerialization.data(withJSONObject:d,options:[.sortedKeys]), let dir = FileManager.default.urls(for:.documentDirectory,in:.userDomainMask).first { try? data.write(to:dir.appendingPathComponent("volume-evidence.json"),options:.atomic) }
  }
}
