#!/usr/bin/env bash
set -euo pipefail
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
swift build --package-path "$repo_root/macos" --configuration release --product AIEmailSearch
binary_dir="$(swift build --package-path "$repo_root/macos" --configuration release --show-bin-path)"
app_dir="$repo_root/macos/build/AI Email.app"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
install -m 0755 "$binary_dir/AIEmailSearch" "$app_dir/Contents/MacOS/AIEmailSearch"
# 图标内容变化时使用新资源名，让系统重新读取图标而非沿用同名缓存。
icon_hash="$(shasum -a 256 "$repo_root/macos/AppIcon.icns" | cut -c1-12)"
icon_name="AppIcon-$icon_hash"
install -m 0644 "$repo_root/macos/AppIcon.icns" "$app_dir/Contents/Resources/$icon_name.icns"
cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>AIEmailSearch</string>
<key>CFBundleIdentifier</key><string>com.aiemail.search.macos</string>
<key>CFBundleName</key><string>AI Email</string>
<key>CFBundleDisplayName</key><string>AI Email</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>CFBundleIconFile</key><string>$icon_name</string>
<key>LSMinimumSystemVersion</key><string>13.0</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSAppTransportSecurity</key><dict>
<key>NSAllowsLocalNetworking</key><true/>
</dict>
<key>NSLocalNetworkUsageDescription</key><string>连接你配置的内网邮件服务器，管理邮箱、搜索和回复邮件。</string>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
# 本机 ad-hoc 签名；正式分发可显式设置稳定的 Developer ID 签名身份。
codesign --force --sign "${MACOS_SIGN_IDENTITY:--}" "$app_dir"
codesign --verify --strict "$app_dir"
printf 'macOS app: %s\n' "$app_dir"
