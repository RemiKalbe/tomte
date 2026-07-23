#!/usr/bin/env bash
# Install a LaunchAgent that runs the BUNDLED chezmoid at login.
# User-invoked only; run scripts/bundle.sh first.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
chezmoid="$repo_root/target/bundle/Chezmoi UI.app/Contents/MacOS/chezmoid"

if [[ ! -x "$chezmoid" ]]; then
    echo "error: bundled chezmoid not found at:" >&2
    echo "  $chezmoid" >&2
    echo "run scripts/bundle.sh first." >&2
    exit 1
fi

label="com.remikalbe.chezmoid"
plist="$HOME/Library/LaunchAgents/$label.plist"
log_dir="$HOME/Library/Application Support/ChezmoiUI"
log_file="$log_dir/chezmoid.launchd.log"

mkdir -p "$HOME/Library/LaunchAgents" "$log_dir"

cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$label</string>
	<key>ProgramArguments</key>
	<array>
		<string>$chezmoid</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>$log_file</string>
	<key>StandardErrorPath</key>
	<string>$log_file</string>
</dict>
</plist>
EOF

launchctl bootout "gui/$UID/$label" 2>/dev/null || true
launchctl bootstrap "gui/$UID" "$plist"

echo "installed and started: $label"
echo "  daemon: $chezmoid"
echo "  logs:   $log_file"
echo
echo "to uninstall:"
echo "  launchctl bootout gui/\$UID/$label"
echo "  rm \"$plist\""
