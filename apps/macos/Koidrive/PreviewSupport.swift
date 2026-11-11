// SPDX-License-Identifier: Apache-2.0

import KoidriveFFI
import KoidriveModel

extension AppModel {
    /// A started model on a fake scenario, for previews.
    @MainActor
    static func preview(_ scenario: FakeScenario) -> AppModel {
        let model = AppModel.fake(scenario: scenario)
        model.start()
        return model
    }

    /// A started model signed in to an account with a single space, Documents, for the views that
    /// drop the space header when there is only one space.
    @MainActor
    static func previewWithOneSpace() -> AppModel {
        let model = AppModel.preview(.firstRun)
        _ = try? model.session.connect(url: "https://box.example.com")
        _ = try? model.session.claim(code: "R3VN-8KPT-Q2ZL", userName: "Gunwoo", deviceName: "Gunwoo's MacBook Pro")
        _ = try? model.session.createSpace(name: "Documents")
        return model
    }
}
