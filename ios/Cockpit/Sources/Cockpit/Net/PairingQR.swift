// CV6-A — QR encode/decode helpers (CoreImage, no new dependencies).
// Reusable by the Mac app (the encoder half) as well as iOS tests.

import CoreImage
import CoreImage.CIFilterBuiltins
import CoreGraphics
import Foundation

public enum PairingQR {

    // MARK: - Encode

    /// Render `payload` as a QR code image (high error-correction level).
    /// Returns nil if CoreImage cannot produce the image (should not happen in practice).
    public static func makeQRImage(_ payload: PairingPayload) -> CGImage? {
        guard let json = payload.jsonString(),
              let data = json.data(using: .utf8)
        else { return nil }

        let filter = CIFilter.qrCodeGenerator()
        filter.setValue(data, forKey: "inputMessage")
        filter.setValue("H", forKey: "inputCorrectionLevel")   // High

        guard let output = filter.outputImage else { return nil }

        // Scale up so the image is legible (10× the raw pixel grid).
        let scaled = output.transformed(by: CGAffineTransform(scaleX: 10, y: 10))
        let ctx = CIContext()
        return ctx.createCGImage(scaled, from: scaled.extent)
    }

    // MARK: - Decode (used in tests; mirrors what AVFoundation reads on device)

    /// Extract the first QR string from a `CGImage` using `CIDetector`.
    /// Returns nil if no QR code is found or the image is nil.
    public static func decodeQR(from cgImage: CGImage) -> String? {
        let ciImage = CIImage(cgImage: cgImage)
        let ctx = CIContext()
        let detector = CIDetector(
            ofType: CIDetectorTypeQRCode,
            context: ctx,
            options: [CIDetectorAccuracy: CIDetectorAccuracyHigh]
        )
        let features = detector?.features(in: ciImage) ?? []
        return (features.first as? CIQRCodeFeature)?.messageString
    }
}
