import Foundation

extension TimeInterval {
    static func seconds(_ value: Double) -> TimeInterval { value }
    static func milliseconds(_ value: Double) -> TimeInterval { value / 1_000 }
}

extension Task where Success == Never, Failure == Never {
    static func sleep(for seconds: TimeInterval) async throws {
        try await sleep(nanoseconds: UInt64(max(0, seconds) * 1_000_000_000))
    }
}

final class LockedState<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Value
    init(initialState: Value) { value = initialState }
    func withLock<Result>(_ body: (inout Value) throws -> Result) rethrows -> Result {
        lock.lock()
        defer { lock.unlock() }
        return try body(&value)
    }
}

func appDurationText(seconds: Int) -> String {
    let formatter = DateComponentsFormatter()
    formatter.allowedUnits = [.hour, .minute, .second]
    formatter.unitsStyle = .abbreviated
    formatter.maximumUnitCount = 1
    return formatter.string(from: TimeInterval(seconds)) ?? String(seconds)
}

extension TimeInterval {
    static func seconds<Value: BinaryInteger>(_ value: Value) -> TimeInterval { Double(value) }
    static func milliseconds<Value: BinaryInteger>(_ value: Value) -> TimeInterval { Double(value) / 1_000 }
}
