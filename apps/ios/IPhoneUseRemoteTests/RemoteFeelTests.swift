import XCTest
@testable import IPhoneUseRemote

final class GestureMapperTests: XCTestCase {
    private func s(_ x: Double, _ y: Double, _ t: TimeInterval) -> GestureMapper.Sample { .init(x: x, y: y, t: t) }

    func testQuickStillTouchIsATap() {
        let a = GestureMapper.action(start: s(0.3, 0.4, 0), end: s(0.3, 0.4, 0.12), movedPoints: 2, heldBeforeMoving: false)
        guard case let .tap(x, y) = a else { return XCTFail("\(a)") }
        XCTAssertEqual(x, 0.3)
        XCTAssertEqual(y, 0.4)
    }

    func testStillHoldIsALongPressOfItsLength() {
        let a = GestureMapper.action(start: s(0.5, 0.5, 0), end: s(0.5, 0.5, 1.4), movedPoints: 3, heldBeforeMoving: true)
        guard case let .longPress(_, _, ms) = a else { return XCTFail("\(a)") }
        XCTAssertEqual(ms, 1400)
    }

    func testMoveAfterHoldIsADragTimedFromTheMove() {
        let a = GestureMapper.action(start: s(0.2, 0.2, 0), end: s(0.6, 0.7, 1.0), movedPoints: 200, heldBeforeMoving: true)
        guard case let .drag(x1, y1, x2, y2, hold, ms) = a else { return XCTFail("\(a)") }
        XCTAssertEqual([x1, y1, x2, y2], [0.2, 0.2, 0.6, 0.7])
        XCTAssertEqual(hold, 500)
        XCTAssertEqual(ms, 500)
    }

    func testSlowSwipeEndsUnderTheFinger() {
        let a = GestureMapper.action(start: s(0.5, 0.8, 0), end: s(0.5, 0.5, 0.6), movedPoints: 250,
                                     heldBeforeMoving: false, releaseVelocity: CGVector(dx: 0, dy: -0.3))
        guard case let .swipe(_, y1, _, y2, ms) = a else { return XCTFail("\(a)") }
        XCTAssertEqual(y1, 0.8)
        XCTAssertEqual(y2, 0.5)
        XCTAssertEqual(ms, 600)
    }

    func testFlickCarriesOnAndStaysFast() {
        let a = GestureMapper.action(start: s(0.5, 0.8, 0), end: s(0.5, 0.6, 0.09), movedPoints: 160,
                                     heldBeforeMoving: false, releaseVelocity: CGVector(dx: 0, dy: -3))
        guard case let .swipe(x1, _, x2, y2, ms) = a else { return XCTFail("\(a)") }
        XCTAssertEqual(x1, x2, "a straight flick stays straight")
        XCTAssertLessThan(y2, 0.6 - 0.2, "the flick travels on past the finger")
        XCTAssertGreaterThanOrEqual(y2, 0)
        XCTAssertLessThanOrEqual(ms, 120)
        XCTAssertGreaterThanOrEqual(ms, GestureMapper.minimumSwipeMs)
    }

    func testFlickNeverLeavesTheScreen() {
        let a = GestureMapper.action(start: s(0.9, 0.2, 0), end: s(0.97, 0.1, 0.06), movedPoints: 80,
                                     heldBeforeMoving: false, releaseVelocity: CGVector(dx: 4, dy: -4))
        guard case let .swipe(_, _, x2, y2, _) = a else { return XCTFail("\(a)") }
        XCTAssertTrue((0...1).contains(x2) && (0...1).contains(y2), "\(x2), \(y2)")
    }

    func testReleaseVelocityUsesTheLastMoments() {
        // Slow for a while, then a quick last 50 ms.
        let samples = [s(0.5, 0.9, 0), s(0.5, 0.88, 0.3), s(0.5, 0.86, 0.6), s(0.5, 0.8, 0.63), s(0.5, 0.7, 0.65)]
        let v = GestureMapper.releaseVelocity(samples)
        XCTAssertLessThan(v.dy, -2, "\(v)")
        XCTAssertEqual(GestureMapper.releaseVelocity([s(0.5, 0.5, 0)]), .zero)
    }
}

final class ZoomStateTests: XCTestCase {
    func testZoomKeepsThePointUnderTheFingers() {
        var zoom = ZoomState()
        let anchor = CGPoint(x: 60, y: -120)
        let before = zoom.unzoomed(anchor)
        zoom.zoom(by: 2, around: anchor)
        let after = zoom.unzoomed(anchor)
        XCTAssertEqual(before.x, after.x, accuracy: 0.001)
        XCTAssertEqual(before.y, after.y, accuracy: 0.001)
        XCTAssertEqual(zoom.scale, 2)
    }

    func testZoomStaysBetweenFitAndTheMaximum() {
        var zoom = ZoomState()
        zoom.zoom(by: 0.5, around: .zero)
        XCTAssertEqual(zoom.scale, 1)
        zoom.zoom(by: 100, around: .zero)
        XCTAssertEqual(zoom.scale, ZoomState.maximum)
    }

    func testPanCannotShowPastAnEdge() {
        var zoom = ZoomState(scale: 2, offset: CGSize(width: 900, height: -900))
        zoom.clamp(content: CGSize(width: 300, height: 650), view: CGSize(width: 390, height: 700))
        // 2× picture is 600×1300 in a 390×700 view: ±105 and ±300.
        XCTAssertEqual(zoom.offset.width, 105, accuracy: 0.001)
        XCTAssertEqual(zoom.offset.height, -300, accuracy: 0.001)
    }

    func testAnAxisSmallerThanTheViewStaysCentered() {
        // A portrait phone in a landscape view: 2× is still narrower.
        var zoom = ZoomState(scale: 2, offset: CGSize(width: 50, height: 50))
        zoom.clamp(content: CGSize(width: 180, height: 390), view: CGSize(width: 844, height: 390))
        XCTAssertEqual(zoom.offset.width, 0)
        XCTAssertEqual(zoom.offset.height, 50)
    }

    func testFitResetsTheOffset() {
        var zoom = ZoomState(scale: 1, offset: CGSize(width: 10, height: 10))
        zoom.clamp(content: CGSize(width: 300, height: 650), view: CGSize(width: 390, height: 700))
        XCTAssertEqual(zoom, ZoomState())
    }
}

final class TypingTests: XCTestCase {
    func testAppendingTypesOnlyTheNewText() {
        XCTAssertEqual(TypingDiff.edit(from: "hel", to: "hello").backspaces, 0)
        XCTAssertEqual(TypingDiff.edit(from: "hel", to: "hello").insert, "lo")
    }

    func testDeletingSendsBackspaces() {
        let edit = TypingDiff.edit(from: "hello", to: "he")
        XCTAssertEqual(edit.backspaces, 3)
        XCTAssertEqual(edit.insert, "")
    }

    func testAReplacedWordBackspacesToTheChangeAndRetypes() {
        let edit = TypingDiff.edit(from: "teh cat", to: "the cat")
        XCTAssertEqual(edit.backspaces, 6)
        XCTAssertEqual(edit.insert, "he cat")
    }

    func testCharactersAreWholeGraphemes() {
        // An emoji with a skin tone is one backspace, as on the phone.
        let edit = TypingDiff.edit(from: "好👍🏽", to: "好")
        XCTAssertEqual(edit.backspaces, 1)
        XCTAssertEqual(TypingDiff.edit(from: "", to: "你好").insert, "你好")
    }

    @MainActor
    func testQueueSendsInOrderAndMergesText() async {
        var sent: [String] = []
        let queue = TypingQueue { action in
            switch action {
            case let .text(text): sent.append("t:" + text)
            case let .key(name): sent.append("k:" + name)
            default: break
            }
            try? await Task.sleep(for: .milliseconds(20))
        }
        queue.enqueue(.text("a"))
        queue.enqueue(.text("b"))
        queue.enqueue(.text("c"))
        queue.enqueue(.key("backspace"))
        queue.enqueue(.text("d"))
        for _ in 0..<100 where queue.pending > 0 { try? await Task.sleep(for: .milliseconds(20)) }
        // Queued before the first send went out: the text is one message.
        XCTAssertEqual(sent, ["t:abc", "k:backspace", "t:d"])
        XCTAssertEqual(queue.sentCharacters, 4)
    }
}

final class VideoPreferenceTests: XCTestCase {
    func testAutoIsQualityOnlyOnALocalRouteOffCellular() {
        XCTAssertTrue(VideoPreference.auto.wantsQuality(localRoute: true, cellular: false))
        XCTAssertFalse(VideoPreference.auto.wantsQuality(localRoute: false, cellular: false))
        XCTAssertFalse(VideoPreference.auto.wantsQuality(localRoute: true, cellular: true))
        XCTAssertTrue(VideoPreference.quality.wantsQuality(localRoute: false, cellular: true))
        XCTAssertFalse(VideoPreference.performance.wantsQuality(localRoute: true, cellular: false))
    }

    func testTheOldSwitchCarriesOver() {
        let defaults = UserDefaults(suiteName: "VideoPreferenceTests")!
        defaults.removePersistentDomain(forName: "VideoPreferenceTests")
        XCTAssertEqual(VideoPreference.load(from: defaults), .auto)
        defaults.set(true, forKey: "videoQuality")
        XCTAssertEqual(VideoPreference.load(from: defaults), .quality)
        defaults.set(VideoPreference.performance.rawValue, forKey: VideoPreference.key)
        XCTAssertEqual(VideoPreference.load(from: defaults), .performance)
        defaults.removePersistentDomain(forName: "VideoPreferenceTests")
    }
}

final class VideoStatePresentationTests: XCTestCase {
    private let t0 = Date(timeIntervalSince1970: 1_000_000)
    private let ready = try! JSONDecoder().decode(PhoneStatus.self, from: Data(#"{"drivable":true,"device_state":"ready"}"#.utf8))

    private func present(_ configure: (inout ConnectionInputs) -> Void) -> ConnectionPresentation {
        var inputs = ConnectionInputs(phase: .connected, status: ready, videoLive: true, now: t0)
        configure(&inputs)
        return ConnectionPresentation.make(inputs)
    }

    func testFreshFramesAreLive() {
        let p = present { $0.lastFrameAt = self.t0.addingTimeInterval(-0.8) }
        XCTAssertEqual(p.placement, .none)
        XCTAssertFalse(p.dimsPicture)
    }

    func testSilenceIsAStallNotAFrozenLivePicture() {
        let p = present { $0.lastFrameAt = self.t0.addingTimeInterval(-4) }
        XCTAssertEqual(p.placement, .banner, "still drivable: a strip, not a card")
        XCTAssertTrue(p.dimsPicture)
        XCTAssertEqual(p.elapsed, 4)
        XCTAssertEqual(p.primary, .reloadVideo)
    }

    func testAReconnectKeepsTheLastPictureWithoutACard() {
        let p = present {
            $0.videoLive = false
            $0.hasPicture = true
            $0.videoWaitSince = self.t0.addingTimeInterval(-2)
        }
        XCTAssertEqual(p.placement, .banner)
        XCTAssertTrue(p.dimsPicture)
    }

    func testNoPictureYetIsStillTheLoadingCard() {
        let p = present { $0.videoLive = false }
        XCTAssertEqual(p.placement, .cover)
    }
}
