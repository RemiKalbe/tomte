# Releasing Tomte

## One-time setup (Apple + GitHub secrets)

Everything below assumes an active Apple Developer Program membership.

**1. Developer ID Application certificate**

- developer.apple.com → Account → Certificates → `+` → **Developer ID Application**.
  (Create the CSR it asks for via Keychain Access → Certificate Assistant →
  Request a Certificate From a Certificate Authority, saved to disk.)
- Download the .cer, double-click to add it to your login keychain.
- Keychain Access → My Certificates → right-click the "Developer ID
  Application: …" entry → Export as `.p12` with a strong password.

```sh
gh secret set MACOS_CERT_P12 --body "$(base64 -i DeveloperID.p12)"
gh secret set MACOS_CERT_PASSWORD   # paste the .p12 password
```

**2. App Store Connect API key** (notarization credentials)

- appstoreconnect.apple.com → Users and Access → Integrations →
  App Store Connect API → Team Keys → `+`, role **Developer**.
- Note the **Key ID** and **Issuer ID**; download `AuthKey_XXXX.p8`
  (downloadable exactly once).

```sh
gh secret set ASC_KEY_ID            # the Key ID
gh secret set ASC_ISSUER_ID         # the Issuer ID
gh secret set ASC_KEY_P8 < AuthKey_XXXX.p8
```

**3. Team ID** (baked into the updater's signature check)

- developer.apple.com → Account → Membership → Team ID (10 chars).
- Set it as a repository **variable** (not secret — it's public knowledge):

```sh
gh variable set APPLE_TEAM_ID --body "XXXXXXXXXX"
```

## Cutting a release

1. Bump `version` in the workspace `Cargo.toml` (single source of truth —
   every crate inherits it, the bundle stamps it, the updater compares it).
2. Commit, then tag and push:

```sh
git tag v0.1.0
git push origin main v0.1.0
```

The Release workflow then: runs the full test suite → builds and signs
`Tomte.app` (hardened runtime) → notarizes with `notarytool` and staples →
publishes `Tomte-X.Y.Z.zip` (what the in-app updater downloads) and
`Tomte-X.Y.Z.dmg` (what humans download) to the GitHub release.

The job refuses to ship if the tag and the workspace version disagree.

## Verifying a shipped bundle locally

```sh
spctl --assess --type execute --verbose /Applications/Tomte.app
codesign --verify --strict --deep --verbose=2 /Applications/Tomte.app
```

Both must pass on a bundle downloaded through a browser (quarantine bit set).
