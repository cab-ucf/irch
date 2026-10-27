#!/bin/sh
# Emulator smoke test (CI): install the APK, launch it, and prove that the
# node starts on a real Android system, the UI works (create a space),
# Back plus relaunch keep the same process with the UI intact, and a call
# starts.
# Usage: smoke.sh APK [EVIDENCE_DIR]
set -eu
apk=$1
out=${2:-evidence}
pkg=dev.irch.chat
mkdir -p "$out"

ui() {
  adb shell uiautomator dump /sdcard/ui.xml >/dev/null 2>&1 || true
  adb shell cat /sdcard/ui.xml | tee "$out/ui.xml"
}
fail() {
  echo "FAIL: $*"
  ui >/dev/null
  adb exec-out screencap -p >"$out/fail.png" || true
  adb logcat -d >"$out/logcat.txt" || true
  exit 1
}
front() {
  adb shell dumpsys activity activities | grep ResumedActivity |
    grep -q "$pkg/"
}
pid() { adb shell pidof "$pkg" || true; }
# Wait up to $2 seconds for text $1 in the accessibility tree (the DOM).
see() {
  i=0
  until ui | grep -q "$1"; do
    i=$((i + 1))
    [ "$i" -lt "$2" ] || return 1
    sleep 1
  done
}
tap() {
  b=$(ui | tr '>' '\n' | grep -m1 -e "text=\"$1\"" -e "content-desc=\"$1\"" |
    grep -o 'bounds="[^"]*"' | tr -c 0-9 ' ')
  [ -n "$b" ] || fail "no \"$1\" on screen"
  set -- $b
  adb shell input tap $((($1 + $3) / 2)) $((($2 + $4) / 2))
}

adb install -r -g "$apk"
# As if Allow was chosen when irch asks to run unrestricted.
adb shell dumpsys deviceidle whitelist +"$pkg" >/dev/null
adb logcat -c
adb shell am start -W -n "$pkg/.MainActivity"
see 'Welcome to irch' 90 || fail "UI never showed the welcome screen"
grep -q 'could not start' "$out/ui.xml" && fail "the client failed to start"
first=$(pid)
[ -n "$first" ] || fail "process died after launch"
adb exec-out screencap -p >"$out/1-launch.png"

# Create a space: storage, signing, sealing and gossip on the device.
tap 'Create a space'
see 'What should people call you' 20 || fail "name dialog did not open"
adb shell input text alice
adb shell input keyevent KEYCODE_ENTER
see 'Customize your space' 20 || fail "create dialog did not open"
adb shell input keyevent KEYCODE_ENTER
see 'general' 30 || fail "space was not created"
adb exec-out screencap -p >"$out/2-space.png"

# Back opens the drawer, then sends irch to the background without stopping it.
adb shell input keyevent KEYCODE_BACK
sleep 2
adb shell input keyevent KEYCODE_BACK
sleep 3
front && fail "Back did not leave the app"
[ "$(pid)" = "$first" ] || fail "Back stopped the process"
adb shell am start -W -n "$pkg/.MainActivity"
see 'general' 30 || fail "UI lost after returning"
front || fail "not in front after returning"
[ "$(pid)" = "$first" ] || fail "returning restarted the process"
adb exec-out screencap -p >"$out/3-resumed.png"

# A call, alone: the web view's codecs, audio worklet and permissions.
tap 'general'
tap 'Join call'
see 'Leave call' 30 || fail "could not join a call"
tap 'Camera'
sleep 5
adb exec-out screencap -p >"$out/5-call.png"

adb logcat -d >"$out/logcat.txt"
# irch's crashes, not the test tools' (uiautomator's own die at times).
if grep -A1 'FATAL EXCEPTION' "$out/logcat.txt" | grep "Process: $pkg" ||
  grep 'panicked at' "$out/logcat.txt"; then
  fail "crash in the device log"
fi
echo "smoke test passed"
