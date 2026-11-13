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
    static let ink = Color(light: NSColor(hex: 0x1D1D1F), dark: NSColor(white: 1, alpha: 0.88))
    static let onInk = Color(light: 0xFFFFFF, dark: 0x1F1F21)
    static let label = Color(light: NSColor(white: 0, alpha: 0.85), dark: NSColor(white: 1, alpha: 0.88))
    static let label3 = Color(light: NSColor(white: 0, alpha: 0.28), dark: NSColor(white: 1, alpha: 0.28))
    static let separatorLine = Color(light: NSColor(white: 0, alpha: 0.1), dark: NSColor(white: 1, alpha: 0.1))
    static let fillStrong = Color(light: NSColor(white: 0, alpha: 0.1), dark: NSColor(white: 1, alpha: 0.14))
    static let accentRing = Color(red: 1, green: 106 / 255, blue: 19 / 255, opacity: 0.35)

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

struct KoiButtonStyle: ButtonStyle {
    enum Kind {
        case primary, secondary, destructiveConfirm
    }

    let kind: Kind

    func makeBody(configuration: Configuration) -> some View {
        KoiButton(kind: kind, configuration: configuration)
    }
}

extension ButtonStyle where Self == KoiButtonStyle {
    static var primary: KoiButtonStyle { KoiButtonStyle(kind: .primary) }
    static var secondary: KoiButtonStyle { KoiButtonStyle(kind: .secondary) }
    static var destructiveConfirm: KoiButtonStyle { KoiButtonStyle(kind: .destructiveConfirm) }
}

private struct KoiButton: View {
    let kind: KoiButtonStyle.Kind
    let configuration: ButtonStyleConfiguration
    @Environment(\.isEnabled) private var isEnabled
    @Environment(\.isFocused) private var isFocused
    @Environment(\.controlSize) private var controlSize

    private var isSmall: Bool { controlSize <= .small }

    var body: some View {
        configuration.label
            .font(isSmall ? .callout : kind == .secondary ? .body : .body.weight(.medium))
            .lineLimit(1)
            .foregroundStyle(labelColor)
            .padding(.horizontal, isSmall ? 10 : 14)
            .frame(height: isSmall ? 20 : 24)
            .background {
                ZStack {
                    if kind == .secondary { Capsule().fill(Color.separatorLine) }
                    Capsule().fill(fillColor).padding(kind == .secondary ? 1 : 0)
                    if kind == .secondary && configuration.isPressed { Capsule().fill(Color.fillStrong).padding(1) }
                }
                .overlay { if isFocused { Capsule().inset(by: -3.5).strokeBorder(Color.accentRing, lineWidth: 3.5) } }
            }
            .opacity(kind != .secondary && !isEnabled ? 0.45 : 1)
            .contentShape(Capsule())
    }

    private var labelColor: Color {
        switch kind {
        case .primary: .onInk
        case .destructiveConfirm: .white
        case .secondary where !isEnabled: .label3
        case .secondary where configuration.role == .destructive: .statusRed
        case .secondary: .label
        }
    }

    private var fillColor: Color {
        switch kind {
        case .primary: .ink.opacity(configuration.isPressed ? 0.8 : 1)
        case .destructiveConfirm: .redFill.opacity(configuration.isPressed ? 0.8 : 1)
        case .secondary: .content
        }
    }
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
