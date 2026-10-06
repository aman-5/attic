import Foundation
import SupportKit

protocol Runnable {
    func run()
}

struct Payload {
    let count: Int
}

class Worker: BaseWorker, Runnable {
    init(name: String) {
        helper(name)
    }

    func run() {
        helper("job")
        describe()
        SupportKit.make("job")
    }

    func describe() {}
}

extension Worker: CustomStringConvertible {
    func pretty() {
        describe()
    }
}

func helper(_ name: String) {}
