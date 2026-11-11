#!/usr/bin/env node
// Plays the KWin cursor script from src/input/linux.rs against stand-ins for KWin: a clock, the
// cursor, the screens, callDBus and QTimer. Nothing here talks to a real KWin.
//
//     node tests/kwin_script_sim.js
'use strict';
const fs = require('fs');
const path = require('path');
const rust = fs.readFileSync(path.join(__dirname, '..', 'src', 'input', 'linux.rs'), 'utf8');
const start = rust.indexOf('const KWIN_SCRIPT: &str = r#"') + 'const KWIN_SCRIPT: &str = r#"'.length;
const script = rust.slice(start, rust.indexOf('"#;', start));

let failures = 0;
function check(name, ok, detail) {
  console.log(`${ok ? 'PASS' : 'FAIL'}: ${name}${ok || detail === undefined ? '' : ` (${detail})`}`);
  if (!ok) failures++;
}

function kwin({ withTimer = true } = {}) {
  const k = { clock: 1000, sent: [], timers: [], moved: [] };
  const pos = { x: 3000, y: 700 };
  class QTimer {
    constructor() { this.singleShot = false; this.interval = 0; this.active = false; const self = this; this.timeout = { connect(f) { self.f = f; } }; k.timers.push(this); }
    start() { this.active = true; this.due = k.clock + this.interval; }
    stop() { this.active = false; }
  }
  const rect = (x, y, width, height) => ({ geometry: { x, y, width, height } });
  const workspace = {
    cursorPos: pos, activeWindow: null,
    screens: [rect(0, 360, 1920, 1080), rect(1920, 0, 2560, 1440)],
    cursorPosChanged: { connect(f) { k.moved.push(f); } },
    windowActivated: { connect() {} },
  };
  const realNow = Date.now;
  Date.now = () => k.clock;
  new Function('workspace', 'callDBus', 'QTimer', script)(workspace, (...a) => k.sent.push(a.slice(4)), withTimer ? QTimer : undefined);
  k.restoreDate = () => { Date.now = realNow; };
  k.tick = (ms) => {
    for (let i = 0; i < ms; i++) {
      k.clock++;
      for (const t of k.timers) {
        if (t.active && k.clock >= t.due) {
          if (t.singleShot) t.active = false; else t.due = k.clock + t.interval;
          t.f();
        }
      }
    }
  };
  k.pos = pos;
  k.moveTo = (x, y) => { pos.x = x; pos.y = y; k.moved.forEach((f) => f()); };
  k.beating = () => k.timers.some((t) => !t.singleShot && t.active);
  k.sentDuring = (ms) => { const before = k.sent.length; k.tick(ms); return k.sent.slice(before); };
  return k;
}

{
  const k = kwin();
  k.tick(50);
  k.moveTo(3000, 700);
  check('away from every edge, no heartbeat', !k.beating());
  check('and a still cursor sends nothing', k.sentDuring(200).length === 0);

  k.moveTo(3000, 1430);
  check('within 16 px of the bottom, the heartbeat runs', k.beating());
  const beats = k.sentDuring(160);
  check('about one position every 16 ms while still', beats.length >= 9 && beats.length <= 11, beats.length);
  check('each with the same place, a new number and a new time',
    beats.every((b) => b[0] === 3000 && b[1] === 1430) && new Set(beats.map((b) => b[3])).size === beats.length
      && new Set(beats.map((b) => b[4])).size === beats.length);
  check('and whole numbers', beats.every((b) => b.every(Number.isInteger)));

  k.moveTo(3000, 1424);
  check('at 15 px from the edge still in the band', k.beating());
  k.moveTo(3000, 1423);
  check('at 16 px out of it, the heartbeat stops', !k.beating());
  k.tick(5);
  check('and a still cursor sends nothing there', k.sentDuring(200).length === 0);

  k.moveTo(100, 365);
  check('the top of the side display is an outer edge too', k.beating());
  k.moveTo(1925, 700);
  check('the join between two screens is no edge', !k.beating());
  k.moveTo(1910, 700);
  check('nor from the other side of it', !k.beating());
  k.moveTo(4475, 700);
  check('the far right is', k.beating());
  k.restoreDate();
}

{
  const k = kwin();
  k.moveTo(3000, 1439);
  check('in the band', k.beating());
  // The cursor leaves with no move reported, as KWin might skip one: the beat itself sees it.
  k.pos.x = 3000;
  k.pos.y = 700;
  k.tick(40);
  check('out of the band, the heartbeat stops by itself', !k.beating());
  check('and sent nothing from out there', k.sentDuring(100).length === 0);
  k.restoreDate();
}

{
  const k = kwin();
  k.moveTo(3000, 1000);
  k.tick(20);
  for (let i = 0; i < 20; i++) { k.moveTo(3000 + 3 * (i + 1), 1000); k.tick(1); }
  k.tick(10);
  const last = k.sent[k.sent.length - 1];
  check('a fast mouse still sends at most one every 2 ms, ending on the latest', k.sent.length <= 13 && last[0] === 3060, `${k.sent.length} ${last}`);
  k.restoreDate();
}

{
  const k = kwin({ withTimer: false });
  k.moveTo(3000, 1430);
  check('a KWin without QTimer sends no heartbeat', k.sentDuring(200).length === 0 && k.timers.length === 0);
  k.restoreDate();
}

console.log(failures ? `${failures} FAILED` : 'ALL PASS');
process.exit(failures ? 1 : 0);
