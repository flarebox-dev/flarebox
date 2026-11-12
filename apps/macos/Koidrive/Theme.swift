// SPDX-License-Identifier: Apache-2.0

import AppKit
import SwiftUI

extension Color {
    static let koi = Color(red: 1, green: 0x6A / 255, blue: 0x13 / 255)
    static let onKoi = Color(red: 0x1A / 255, green: 0x0A / 255, blue: 0)
    static let koiText = Color(light: 0xC2480A, dark: 0xFF8F4D)
    static let koiTint = Color(light: NSColor(red: 1, green: 106 / 255, blue: 19 / 255, alpha: 0.12),
                               dark: NSColor(red: 1, green: 106 / 255, blue: 19 / 255, alpha: 0.18))
    static let statusGreen = Color(light: 0x1FA83A, dark: 0x32D74B)
    static let statusRed = Color(light: 0xE0352B, dark: 0xFF453A)
    static let statusYellow = Color(light: 0xB07A00, dark: 0xFFD60A)
    static let redTint = Color(light: NSColor(red: 224 / 255, green: 53 / 255, blue: 43 / 255, alpha: 0.1),
                               dark: NSColor(red: 1, green: 69 / 255, blue: 58 / 255, alpha: 0.16))
    static let redFill = Color(red: 0xD7 / 255, green: 0, blue: 0x15 / 255)
    static let content = Color(light: 0xFFFFFF, dark: 0x2C2C2E)

    init(light: Int, dark: Int) {
        self.init(light: NSColor(hex: light), dark: NSColor(hex: dark))
    }

    init(light: NSColor, dark: NSColor) {
        self.init(nsColor: NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua ? dark : light
        })
    }
}

extension NSColor {
    convenience init(hex: Int) {
        self.init(srgbRed: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255,
                  blue: CGFloat(hex & 0xFF) / 255, alpha: 1)
    }
}

struct ProminentButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.body.weight(.medium))
            .foregroundStyle(Color.onKoi)
            .padding(.horizontal, 12)
            .padding(.vertical, 4)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color.koi.opacity(configuration.isPressed ? 0.8 : 1)))
            .opacity(isEnabled ? 1 : 0.45)
    }
}

extension ButtonStyle where Self == ProminentButtonStyle {
    static var prominent: ProminentButtonStyle { ProminentButtonStyle() }
}

struct CountBadge: View {
    let count: Int
    let fill: Color
    let text: Color

    var body: some View {
        Text(count.formatted())
            .font(.caption.weight(.bold))
            .monospacedDigit()
            .foregroundStyle(text)
            .padding(.horizontal, 6)
            .frame(minWidth: 18, minHeight: 16)
            .background(Capsule().fill(fill))
    }
}

struct ErrorBanner: View {
    let message: String

    var body: some View {
        Label(message, systemImage: "exclamationmark.triangle")
            .foregroundStyle(Color.statusRed)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 8)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.redTint))
    }
}

struct RowGroup<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            content
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Color.content))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(.separator))
    }
}
