#!/usr/bin/env bash
# Install a LaunchAgent that runs the BUNDLED tomted at login.
# User-invoked only; run scripts/bundle.sh first.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tomted="$repo_root/target/bundle/Tomte.app/Contents/MacOS/tomted"

if [[ ! -x "$tomted" ]]; then
    echo "error: bundled tomted not found at:" >&2
    echo "  $tomted" >&2
    echo "run scripts/bundle.sh first." >&2
    exit 1
fi

label="com.remikalbe.tomted"
plist="$HOME/Library/LaunchAgents/$label.plist"
log_dir="$HOME/Library/Application Support/Tomte"
log_file="$log_dir/tomted.launchd.log"

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
		<string>$tomted</string>
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
echo "  daemon: $tomted"
echo "  logs:   $log_file"
echo
echo "to uninstall:"
echo "  launchctl bootout gui/\$UID/$label"
echo "  rm \"$plist\""
