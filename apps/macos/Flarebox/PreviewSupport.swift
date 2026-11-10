// SPDX-License-Identifier: Apache-2.0

import FlareboxFFI
import FlareboxModel

extension AppModel {
    /// A started model on a fake scenario, for previews.
    @MainActor
    static func preview(_ scenario: FakeScenario) -> AppModel {
        let model = AppModel.fake(scenario: scenario)
        model.start()
        return model
    }
}
