#!/usr/bin/env python3
import json
import math
import sys
import urllib.request
from PyQt6.QtCore import Qt, QTimer, QRectF, QPointF
from PyQt6.QtGui import (
    QColor,
    QFont,
    QLinearGradient,
    QPainter,
    QPainterPath,
    QPen,
    QBrush,
)
from PyQt6.QtWidgets import (
    QApplication,
    QCheckBox,
    QComboBox,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QMainWindow,
    QProgressBar,
    QPushButton,
    QScrollArea,
    QSlider,
    QVBoxLayout,
    QWidget,
)

API_BASE = "http://127.0.0.1:24880"

DARK_STYLESHEET = """
QMainWindow, QWidget {
    background-color: #0b0f17;
    color: #f1f5f9;
    font-family: "Inter", "Noto Sans", "Segoe UI", sans-serif;
    font-size: 13px;
}
QFrame#card {
    background-color: #131b2b;
    border: 1px solid #24324b;
    border-radius: 10px;
}
QFrame#headerBar {
    background-color: #111827;
    border-bottom: 1px solid #24324b;
}
QLabel#sectionTitle {
    color: #38bdf8;
    font-weight: 700;
    font-size: 12px;
    letter-spacing: 0.6px;
}
QLabel#muted {
    color: #94a3b8;
    font-size: 12px;
}
QPushButton {
    background-color: #1e293b;
    color: #f1f5f9;
    border: 1px solid #334155;
    border-radius: 7px;
    padding: 7px 11px;
    font-weight: 600;
}
QPushButton:hover {
    background-color: #2b3b54;
    border-color: #38bdf8;
}
QPushButton:pressed {
    background-color: #0f172a;
}
QPushButton#primaryBtn {
    background-color: #0284c7;
    border: 1px solid #38bdf8;
    color: #ffffff;
}
QPushButton#primaryBtn:hover {
    background-color: #0369a1;
}
QPushButton#activeToggleBtn {
    background-color: #059669;
    border: 1px solid #34d399;
    color: #ffffff;
}
QPushButton#dangerBtn {
    background-color: #3f151b;
    border: 1px solid #ef4444;
    color: #fca5a5;
}
QPushButton#dangerBtn:hover {
    background-color: #5c1c24;
}
QComboBox {
    background-color: #0f172a;
    border: 1px solid #334155;
    border-radius: 6px;
    padding: 5px 10px;
    color: #f8fafc;
    font-weight: 600;
}
QComboBox:hover {
    border-color: #38bdf8;
}
QComboBox QAbstractItemView {
    background-color: #0f172a;
    color: #f8fafc;
    selection-background-color: #0284c7;
    border: 1px solid #38bdf8;
}
QSlider::groove:horizontal {
    border: 1px solid #1e293b;
    height: 6px;
    background: #0f172a;
    border-radius: 3px;
}
QSlider::sub-page:horizontal {
    background: #0284c7;
    border-radius: 3px;
}
QSlider::handle:horizontal {
    background: #38bdf8;
    border: 2px solid #ffffff;
    width: 15px;
    margin: -5px 0;
    border-radius: 8px;
}
QProgressBar {
    background-color: #090d14;
    border: 1px solid #1e293b;
    border-radius: 4px;
    height: 10px;
    text-align: center;
}
QProgressBar::chunk {
    background-color: qlineargradient(x1:0, y1:0, x2:1, y2:0, stop:0 #10b981, stop:0.7 #38bdf8, stop:1 #f59e0b);
    border-radius: 3px;
}
QCheckBox {
    spacing: 8px;
    color: #e2e8f0;
}
"""


class SpatialCanvasWidget(QWidget):
    """Hardware-accelerated 2D Spatial Monitor & Border Transfer Canvas."""

    def __init__(self, parent_studio):
        super().__init__()
        self.studio = parent_studio
        self.setMinimumSize(640, 460)
        self.setMouseTracking(True)

        self.dragging_win = False
        self.drag_offset_world_x = 0.0
        self.drag_offset_world_y = 0.0
        self.hover_win = False

        self.view_scale = 0.12
        self.view_ox = 120.0
        self.view_oy = 120.0

    def compute_viewport(self):
        st = self.studio.state
        if not st:
            return
        c = st["config"]
        min_x = float(c["win_x"])
        min_y = float(c["win_y"])
        max_x = float(c["win_x"] + c["win_w"])
        max_y = float(c["win_y"] + c["win_h"])

        for m in c["debian_monitors"]:
            min_x = min(min_x, float(m["x"]))
            min_y = min(min_y, float(m["y"]))
            max_x = max(max_x, float(m["x"] + m["w"]))
            max_y = max(max_y, float(m["y"] + m["h"]))

        world_w = max(2400.0, max_x - min_x)
        world_h = max(1600.0, max_y - min_y)

        avail_w = max(200.0, self.width() - 96.0)
        avail_h = max(200.0, self.height() - 96.0)

        self.view_scale = min(avail_w / world_w, avail_h / world_h, 0.22)
        self.view_ox = (self.width() - world_w * self.view_scale) * 0.5 - min_x * self.view_scale
        self.view_oy = (self.height() - world_h * self.view_scale) * 0.5 - min_y * self.view_scale

    def to_screen(self, wx, wy):
        return (
            self.view_ox + wx * self.view_scale,
            self.view_oy + wy * self.view_scale,
        )

    def to_world(self, sx, sy):
        return (
            (sx - self.view_ox) / max(1e-5, self.view_scale),
            (sy - self.view_oy) / max(1e-5, self.view_scale),
        )

    def mousePressEvent(self, event):
        if event.button() != Qt.MouseButton.LeftButton or not self.studio.state:
            return
        self.compute_viewport()
        pos = event.position()
        wx, wy = self.to_world(pos.x(), pos.y())
        c = self.studio.state["config"]
        if (
            c["win_x"] <= wx <= c["win_x"] + c["win_w"]
            and c["win_y"] <= wy <= c["win_y"] + c["win_h"]
        ):
            self.dragging_win = True
            self.drag_offset_world_x = wx - c["win_x"]
            self.drag_offset_world_y = wy - c["win_y"]
            self.setCursor(Qt.CursorShape.ClosedHandCursor)

    def mouseMoveEvent(self, event):
        if not self.studio.state:
            return
        self.compute_viewport()
        pos = event.position()
        wx, wy = self.to_world(pos.x(), pos.y())
        c = self.studio.state["config"]

        if self.dragging_win:
            raw_x = wx - self.drag_offset_world_x
            raw_y = wy - self.drag_offset_world_y
            snapped = self.studio.snap_windows_box(
                raw_x, raw_y, c["win_w"], c["win_h"], c["debian_monitors"]
            )
            if snapped:
                c["win_x"], c["win_y"], c["attach_side"] = snapped
                self.studio.update_status_labels()
                self.update()
        else:
            inside = (
                c["win_x"] <= wx <= c["win_x"] + c["win_w"]
                and c["win_y"] <= wy <= c["win_y"] + c["win_h"]
            )
            if inside != self.hover_win:
                self.hover_win = inside
                self.setCursor(
                    Qt.CursorShape.OpenHandCursor
                    if inside
                    else Qt.CursorShape.ArrowCursor
                )
                self.update()

    def mouseReleaseEvent(self, event):
        if event.button() == Qt.MouseButton.LeftButton and self.dragging_win:
            self.dragging_win = False
            self.setCursor(Qt.CursorShape.OpenHandCursor)
            self.studio.push_layout(status_msg="✓ Snapped & applied new 2D display position!")
            self.update()

    def paintEvent(self, _event):
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        p.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)

        # Stage background
        bg_rect = QRectF(0, 0, self.width(), self.height())
        p.fillRect(bg_rect, QColor("#0d131f"))

        # Subtle coordinate grid
        grid_pen = QPen(QColor("#172236"), 1.0)
        p.setPen(grid_pen)
        step = 28
        for x in range(0, self.width(), step):
            p.drawLine(x, 0, x, self.height())
        for y in range(0, self.height(), step):
            p.drawLine(0, y, self.width(), y)

        # Stage border
        p.setPen(QPen(QColor("#24324b"), 1.5))
        p.drawRoundedRect(bg_rect.adjusted(1, 1, -1, -1), 10, 10)

        st = self.studio.state
        if not st:
            p.setPen(QColor("#94a3b8"))
            p.setFont(QFont("Inter", 13))
            p.drawText(bg_rect, Qt.AlignmentFlag.AlignCenter, "Connecting to mesh-kvm daemon (127.0.0.1:24880)...")
            return

        self.compute_viewport()
        c = st["config"]
        t = st["telemetry"]
        deb_active = t.get("active_node", "Debian") == "Debian"
        game_locked = bool(t.get("game_captive_active", False))

        # 1. Draw Debian Hardware Monitors
        for m in c["debian_monitors"]:
            sx, sy = self.to_screen(m["x"], m["y"])
            sw = m["w"] * self.view_scale
            sh = m["h"] * self.view_scale
            rect = QRectF(sx, sy, sw, sh)

            grad = QLinearGradient(sx, sy, sx, sy + sh)
            if deb_active:
                grad.setColorAt(0.0, QColor(16, 185, 129, 68))
                grad.setColorAt(1.0, QColor(6, 95, 70, 88))
                border_col = QColor("#38bdf8")
                border_w = 2.8
            else:
                grad.setColorAt(0.0, QColor(16, 185, 129, 36))
                grad.setColorAt(1.0, QColor(15, 23, 42, 160))
                border_col = QColor("#10b981")
                border_w = 1.8

            path = QPainterPath()
            path.addRoundedRect(rect, 9, 9)
            p.fillPath(path, QBrush(grad))
            p.setPen(QPen(border_col, border_w))
            p.drawPath(path)

            # Monitor title & native resolution info
            p.setPen(QColor("#f8fafc"))
            p.setFont(QFont("Inter", 10, QFont.Weight.Bold))
            title_rect = QRectF(sx + 6, sy + sh * 0.22, sw - 12, 22)
            p.drawText(title_rect, Qt.AlignmentFlag.AlignCenter, f"🖥 Debian: {m['name']}")

            p.setPen(QColor("#a7f3d0"))
            p.setFont(QFont("Monospace", 9, QFont.Weight.DemiBold))
            sub_rect = QRectF(sx + 6, sy + sh * 0.22 + 22, sw - 12, 18)
            p.drawText(
                sub_rect,
                Qt.AlignmentFlag.AlignCenter,
                f"{m['w']} × {m['h']}  @ ({m['x']}, {m['y']})",
            )

            p.setPen(QColor("#6ee7b7"))
            p.setFont(QFont("Inter", 8))
            hw_rect = QRectF(sx + 6, sy + sh * 0.22 + 40, sw - 12, 16)
            p.drawText(hw_rect, Qt.AlignmentFlag.AlignCenter, "Physical USB Mouse Host")

        # 2. Draw Windows 11 Laptop Monitor
        wx, wy, ww, wh = c["win_x"], c["win_y"], c["win_w"], c["win_h"]
        wsx, wsy = self.to_screen(wx, wy)
        wsw = ww * self.view_scale
        wsh = wh * self.view_scale
        wrect = QRectF(wsx, wsy, wsw, wsh)

        wgrad = QLinearGradient(wsx, wsy, wsx, wsy + wsh)
        if not deb_active:
            wgrad.setColorAt(0.0, QColor(99, 102, 241, 95))
            wgrad.setColorAt(1.0, QColor(55, 48, 163, 115))
            wborder_col = QColor("#38bdf8")
            wborder_w = 3.0
        elif self.hover_win or self.dragging_win:
            wgrad.setColorAt(0.0, QColor(99, 102, 241, 80))
            wgrad.setColorAt(1.0, QColor(67, 56, 202, 95))
            wborder_col = QColor("#a5b4fc")
            wborder_w = 2.4
        else:
            wgrad.setColorAt(0.0, QColor(99, 102, 241, 55))
            wgrad.setColorAt(1.0, QColor(30, 27, 75, 140))
            wborder_col = QColor("#6366f1")
            wborder_w = 2.0

        wpath = QPainterPath()
        wpath.addRoundedRect(wrect, 9, 9)
        p.fillPath(wpath, QBrush(wgrad))
        p.setPen(QPen(wborder_col, wborder_w))
        p.drawPath(wpath)

        mode_badge = "NATIVE" if c.get("resolution_mode", "native") == "native" else "CUSTOM"
        p.setPen(QColor("#ffffff"))
        p.setFont(QFont("Inter", 10, QFont.Weight.Bold))
        p.drawText(
            QRectF(wsx + 6, wsy + wsh * 0.18, wsw - 12, 22),
            Qt.AlignmentFlag.AlignCenter,
            "💻 Windows 11 Laptop",
        )

        p.setPen(QColor("#e0e7ff"))
        p.setFont(QFont("Monospace", 9, QFont.Weight.Bold))
        p.drawText(
            QRectF(wsx + 6, wsy + wsh * 0.18 + 20, wsw - 12, 18),
            Qt.AlignmentFlag.AlignCenter,
            f"{ww} × {wh} [{mode_badge}]",
        )

        p.setPen(QColor("#c7d2fe"))
        p.setFont(QFont("Inter", 8))
        p.drawText(
            QRectF(wsx + 6, wsy + wsh * 0.18 + 38, wsw - 12, 18),
            Qt.AlignmentFlag.AlignCenter,
            f"Pos ({wx}, {wy}) • Drag to any border",
        )

        # 3. Draw Active Shared Border Transfer Portal Bar
        side = c["attach_side"]
        deb_w = c.get("deb_total_w", 4480)
        deb_h = c.get("deb_total_h", 1440)
        portal_col = QColor("#f59e0b") if game_locked else QColor("#22d3ee")

        if side == "Right":
            by1, by2 = max(0, wy), min(deb_h, wy + wh)
            sx1, sy1 = self.to_screen(wx, by1)
            _, sy2 = self.to_screen(wx, by2)
            p.setPen(QPen(portal_col, 5.0, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap))
            p.drawLine(QPointF(sx1, sy1), QPointF(sx1, sy2))
        elif side == "Left":
            bx = wx + ww
            by1, by2 = max(0, wy), min(deb_h, wy + wh)
            sx1, sy1 = self.to_screen(bx, by1)
            _, sy2 = self.to_screen(bx, by2)
            p.setPen(QPen(portal_col, 5.0, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap))
            p.drawLine(QPointF(sx1, sy1), QPointF(sx1, sy2))
        elif side == "Bottom":
            bx1, bx2 = max(0, wx), min(deb_w, wx + ww)
            sx1, sy1 = self.to_screen(bx1, wy)
            sx2, _ = self.to_screen(bx2, wy)
            p.setPen(QPen(portal_col, 5.0, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap))
            p.drawLine(QPointF(sx1, sy1), QPointF(sx2, sy1))
        elif side == "Top":
            by = wy + wh
            bx1, bx2 = max(0, wx), min(deb_w, wx + ww)
            sx1, sy1 = self.to_screen(bx1, by)
            sx2, _ = self.to_screen(bx2, by)
            p.setPen(QPen(portal_col, 5.0, Qt.PenStyle.SolidLine, Qt.PenCapStyle.RoundCap))
            p.drawLine(QPointF(sx1, sy1), QPointF(sx2, sy1))

        # 4. Draw Live Cursor Position Dot
        cur_world_x = t["deb_cursor_x"] if deb_active else (wx + t["win_cursor_x"])
        cur_world_y = t["deb_cursor_y"] if deb_active else (wy + t["win_cursor_y"])
        csx, csy = self.to_screen(cur_world_x, cur_world_y)

        # Outer halo
        p.setPen(Qt.PenStyle.NoPen)
        p.setBrush(QColor(244, 63, 94, 90))
        p.drawEllipse(QPointF(csx, csy), 11.0, 11.0)

        # Inner cursor dot
        p.setPen(QPen(QColor("#ffffff"), 2.0))
        p.setBrush(QColor("#f43f5e"))
        p.drawEllipse(QPointF(csx, csy), 5.5, 5.5)

        # Legend at bottom-left of canvas
        p.setPen(QColor("#94a3b8"))
        p.setFont(QFont("Inter", 9))
        legend = (
            "🔒 Amber Border = Game Captive Locked (Mouse stays inside screen)"
            if game_locked
            else "✨ Cyan Border = Active Mouse Transfer Portal   •   🔴 Red Dot = Live Cursor"
        )
        p.drawText(QRectF(14, self.height() - 26, self.width() - 28, 20), Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter, legend)


class MeshKvmStudio(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("mesh-kvm Studio — Spatial Monitors, Mouse Calibration & Audio Bus")
        self.resize(1260, 800)

        self.state = None
        self.updating_ui_programmatically = False

        # Debounce timer for live slider updates
        self.push_timer = QTimer(self)
        self.push_timer.setSingleShot(True)
        self.push_timer.timeout.connect(self._do_push_layout)

        root = QWidget()
        self.setCentralWidget(root)
        root_vbox = QVBoxLayout(root)
        root_vbox.setContentsMargins(0, 0, 0, 0)
        root_vbox.setSpacing(0)

        # Top Header Bar
        header = QFrame()
        header.setObjectName("headerBar")
        header_hbox = QHBoxLayout(header)
        header_hbox.setContentsMargins(18, 10, 18, 10)
        header_hbox.setSpacing(14)

        brand_lbl = QLabel("⚡ mesh-kvm Unified Control Studio")
        brand_lbl.setFont(QFont("Inter", 14, QFont.Weight.Bold))
        header_hbox.addWidget(brand_lbl)

        self.active_badge = QLabel("Active Focus: Debian")
        self.active_badge.setStyleSheet(
            "background-color: rgba(16, 185, 129, 0.2); color: #34d399; border: 1px solid #059669; border-radius: 12px; padding: 4px 12px; font-weight: 700;"
        )
        header_hbox.addWidget(self.active_badge)

        self.peer_badge = QLabel("🟢 Laptop Connected")
        self.peer_badge.setStyleSheet(
            "background-color: rgba(16, 185, 129, 0.16); color: #6ee7b7; border: 1px solid #059669; border-radius: 12px; padding: 4px 12px; font-weight: 600;"
        )
        header_hbox.addWidget(self.peer_badge)

        self.game_badge = QLabel("🎮 Game Lock: Auto-Standby")
        self.game_badge.setStyleSheet(
            "background-color: rgba(51, 65, 85, 0.5); color: #cbd5e1; border: 1px solid #475569; border-radius: 12px; padding: 4px 12px; font-weight: 600;"
        )
        header_hbox.addWidget(self.game_badge)

        header_hbox.addStretch(1)

        self.cursor_readout = QLabel("Cursor: (0, 0)")
        self.cursor_readout.setFont(QFont("Monospace", 10))
        self.cursor_readout.setStyleSheet("color: #94a3b8;")
        header_hbox.addWidget(self.cursor_readout)

        switch_btn = QPushButton("⇄ Switch Focus (Ctrl+Alt+F11)")
        switch_btn.clicked.connect(lambda: self.post_action("toggle_focus"))
        header_hbox.addWidget(switch_btn)

        panic_btn = QPushButton("⚠ Panic Unlock (Ctrl+Alt+F12)")
        panic_btn.setObjectName("dangerBtn")
        panic_btn.clicked.connect(lambda: self.post_action("panic"))
        header_hbox.addWidget(panic_btn)

        root_vbox.addWidget(header)

        # Main Content Split: Left (2D Canvas + Status) | Right (Control Cards)
        body_hbox = QHBoxLayout()
        body_hbox.setContentsMargins(14, 14, 14, 14)
        body_hbox.setSpacing(14)
        root_vbox.addLayout(body_hbox, 1)

        # Left Column
        left_vbox = QVBoxLayout()
        left_vbox.setSpacing(8)
        self.canvas = SpatialCanvasWidget(self)
        left_vbox.addWidget(self.canvas, 1)

        self.status_bar_lbl = QLabel(
            "Drag the '💻 Windows 11 Laptop' display onto any edge of your Debian monitors. All changes apply live."
        )
        self.status_bar_lbl.setStyleSheet(
            "background-color: #131b2b; border: 1px solid #24324b; border-radius: 8px; padding: 8px 12px; color: #38bdf8; font-weight: 600;"
        )
        left_vbox.addWidget(self.status_bar_lbl)
        body_hbox.addLayout(left_vbox, 1)

        # Right Scrollable Sidebar
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFixedWidth(420)
        scroll.setFrameShape(QFrame.Shape.NoFrame)

        sidebar_widget = QWidget()
        sidebar = QVBoxLayout(sidebar_widget)
        sidebar.setContentsMargins(0, 0, 4, 0)
        sidebar.setSpacing(12)
        scroll.setWidget(sidebar_widget)
        body_hbox.addWidget(scroll)

        # --- CARD 1: DISPLAY RESOLUTION & SPATIAL SNAP ---
        card_disp = QFrame()
        card_disp.setObjectName("card")
        v_disp = QVBoxLayout(card_disp)
        v_disp.setContentsMargins(14, 12, 14, 12)
        v_disp.setSpacing(9)

        lbl_c1 = QLabel("1. DISPLAY RESOLUTIONS & 2D BORDER POSITION")
        lbl_c1.setObjectName("sectionTitle")
        v_disp.addWidget(lbl_c1)

        self.deb_native_lbl = QLabel("Debian Native: DP-3 (1920×1080) + DP-2 (2560×1440)")
        self.deb_native_lbl.setObjectName("muted")
        v_disp.addWidget(self.deb_native_lbl)

        res_row = QHBoxLayout()
        res_row.addWidget(QLabel("Laptop Resolution:"))
        self.res_combo = QComboBox()
        self.res_options = [
            ("native", "Native Auto-Detect (Default)"),
            ("1920x1080", "1920 × 1080 (FHD 16:9)"),
            ("1920x1200", "1920 × 1200 (FHD+ 16:10)"),
            ("2560x1440", "2560 × 1440 (QHD 16:9)"),
            ("2560x1600", "2560 × 1600 (QHD+ 16:10)"),
            ("2880x1800", "2880 × 1800 (2.8K 16:10)"),
            ("3840x2160", "3840 × 2160 (4K UHD)"),
        ]
        for val, label in self.res_options:
            self.res_combo.addItem(label, val)
        self.res_combo.currentIndexChanged.connect(self.on_res_combo_changed)
        res_row.addWidget(self.res_combo, 1)
        v_disp.addLayout(res_row)

        reset_native_btn = QPushButton("🔄 Auto-Detect & Reset All Displays to Native")
        reset_native_btn.clicked.connect(self.on_reset_native_clicked)
        v_disp.addWidget(reset_native_btn)

        snap_grid = QGridLayout()
        snap_grid.setSpacing(6)
        presets = [
            ("⬇ Below Main (DP-2)", "below_dp2", 0, 0),
            ("➡ Right of Main (DP-2)", "right_dp2", 0, 1),
            ("⬇ Below Left (DP-3)", "below_dp3", 1, 0),
            ("⬅ Left of Left (DP-3)", "left_dp3", 1, 1),
            ("⬆ Above Main (DP-2)", "above_dp2", 2, 0),
            ("⬆ Above Left (DP-3)", "above_dp3", 2, 1),
        ]
        for text, code, r, col in presets:
            b = QPushButton(text)
            b.clicked.connect(lambda _checked=False, cd=code: self.apply_preset(cd))
            snap_grid.addWidget(b, r, col)
        v_disp.addLayout(snap_grid)

        self.border_readout_lbl = QLabel("Attached Border: Bottom  |  Coords: (2240, 1440)")
        self.border_readout_lbl.setFont(QFont("Monospace", 9, QFont.Weight.Bold))
        self.border_readout_lbl.setStyleSheet("color: #a5b4fc;")
        v_disp.addWidget(self.border_readout_lbl)

        sidebar.addWidget(card_disp)

        # --- CARD 2: WINDOWS MOUSE SENSITIVITY & EDGE RESISTANCE ---
        card_mouse = QFrame()
        card_mouse.setObjectName("card")
        v_mouse = QVBoxLayout(card_mouse)
        v_mouse.setContentsMargins(14, 12, 14, 12)
        v_mouse.setSpacing(8)

        lbl_c2 = QLabel("2. WINDOWS MOUSE SENSITIVITY & DPI CALIBRATION")
        lbl_c2.setObjectName("sectionTitle")
        v_mouse.addWidget(lbl_c2)

        spd_top = QHBoxLayout()
        spd_top.addWidget(QLabel("Windows Pointer Speed:"))
        self.speed_val_lbl = QLabel("0.35× (35%)")
        self.speed_val_lbl.setStyleSheet("color: #38bdf8; font-weight: 700;")
        spd_top.addStretch(1)
        spd_top.addWidget(self.speed_val_lbl)
        v_mouse.addLayout(spd_top)

        # Slider 5..150 representing 0.05x .. 1.50x
        self.speed_slider = QSlider(Qt.Orientation.Horizontal)
        self.speed_slider.setRange(5, 150)
        self.speed_slider.setSingleStep(5)
        self.speed_slider.setValue(35)
        self.speed_slider.valueChanged.connect(self.on_speed_slider_changed)
        v_mouse.addWidget(self.speed_slider)

        # Quick DPI calibration buttons
        dpi_row = QHBoxLayout()
        dpi_row.setSpacing(5)
        for label, mult in [
            ("3200 DPI (0.20×)", 20),
            ("1600 DPI (0.35×)", 35),
            ("800 DPI (0.65×)", 65),
            ("400 DPI (1.00×)", 100),
        ]:
            btn = QPushButton(label)
            btn.setStyleSheet("font-size: 11px; padding: 5px 6px;")
            btn.clicked.connect(lambda _c=False, m=mult: self.speed_slider.setValue(m))
            dpi_row.addWidget(btn)
        v_mouse.addLayout(dpi_row)

        res_top = QHBoxLayout()
        res_top.addWidget(QLabel("Border Crossing Push Resistance:"))
        self.resist_val_lbl = QLabel("55 px")
        self.resist_val_lbl.setStyleSheet("color: #cbd5e1; font-weight: 600;")
        res_top.addStretch(1)
        res_top.addWidget(self.resist_val_lbl)
        v_mouse.addLayout(res_top)

        self.resist_slider = QSlider(Qt.Orientation.Horizontal)
        self.resist_slider.setRange(10, 180)
        self.resist_slider.setSingleStep(5)
        self.resist_slider.setValue(55)
        self.resist_slider.valueChanged.connect(self.on_resist_slider_changed)
        v_mouse.addWidget(self.resist_slider)

        sidebar.addWidget(card_mouse)

        # --- CARD 3: AUDIO SHARE & ROUTING STUDIO (UDP 24851) ---
        card_audio = QFrame()
        card_audio.setObjectName("card")
        v_audio = QVBoxLayout(card_audio)
        v_audio.setContentsMargins(14, 12, 14, 12)
        v_audio.setSpacing(9)

        lbl_c3 = QLabel("3. BIDIRECTIONAL AUDIO SHARE STUDIO (UDP 24851)")
        lbl_c3.setObjectName("sectionTitle")
        v_audio.addWidget(lbl_c3)

        # Direction A: Windows 11 Laptop -> Debian Speakers
        rx_title_row = QHBoxLayout()
        rx_lbl = QLabel("💻 Windows 11 ➔ 🖥 Debian Speakers")
        rx_lbl.setStyleSheet("font-weight: 700; color: #e2e8f0;")
        rx_title_row.addWidget(rx_lbl)
        rx_title_row.addStretch(1)
        self.rx_mute_btn = QPushButton("🔊 Active")
        self.rx_mute_btn.clicked.connect(lambda: self.post_action("toggle_audio_mute"))
        rx_title_row.addWidget(self.rx_mute_btn)
        v_audio.addLayout(rx_title_row)

        self.rx_status_lbl = QLabel("Listening on UDP 24851 -> PipeWire 48kHz Stereo")
        self.rx_status_lbl.setObjectName("muted")
        v_audio.addWidget(self.rx_status_lbl)

        self.rx_vu_bar = QProgressBar()
        self.rx_vu_bar.setRange(0, 100)
        self.rx_vu_bar.setValue(0)
        self.rx_vu_bar.setTextVisible(False)
        v_audio.addWidget(self.rx_vu_bar)

        rx_vol_row = QHBoxLayout()
        rx_vol_row.addWidget(QLabel("Incoming Volume:"))
        self.rx_vol_slider = QSlider(Qt.Orientation.Horizontal)
        self.rx_vol_slider.setRange(0, 150)
        self.rx_vol_slider.setValue(100)
        self.rx_vol_slider.valueChanged.connect(self.on_rx_vol_changed)
        rx_vol_row.addWidget(self.rx_vol_slider, 1)
        self.rx_vol_lbl = QLabel("100%")
        self.rx_vol_lbl.setFixedWidth(42)
        rx_vol_row.addWidget(self.rx_vol_lbl)
        v_audio.addLayout(rx_vol_row)

        sep = QFrame()
        sep.setFrameShape(QFrame.Shape.HLine)
        sep.setStyleSheet("color: #24324b;")
        v_audio.addWidget(sep)

        # Direction B: Debian Desktop -> Windows 11 Laptop
        tx_lbl = QLabel("🖥 Share Debian Audio ➔ 💻 Windows 11 Laptop")
        tx_lbl.setStyleSheet("font-weight: 700; color: #e2e8f0;")
        v_audio.addWidget(tx_lbl)

        self.tx_toggle_btn = QPushButton("⚪ Share Debian Audio to Windows: OFF")
        self.tx_toggle_btn.clicked.connect(lambda: self.post_action("toggle_audio_tx"))
        v_audio.addWidget(self.tx_toggle_btn)

        src_row = QHBoxLayout()
        src_row.addWidget(QLabel("Capture Source:"))
        self.tx_source_combo = QComboBox()
        self.tx_source_combo.addItem("🔊 Desktop System Audio (PipeWire Loopback)", 0)
        self.tx_source_combo.addItem("🎙 Microphone Input (USB Audio Mic)", 1)
        self.tx_source_combo.currentIndexChanged.connect(self.on_tx_source_changed)
        src_row.addWidget(self.tx_source_combo, 1)
        v_audio.addLayout(src_row)

        self.tx_vu_bar = QProgressBar()
        self.tx_vu_bar.setRange(0, 100)
        self.tx_vu_bar.setValue(0)
        self.tx_vu_bar.setTextVisible(False)
        v_audio.addWidget(self.tx_vu_bar)

        tx_vol_row = QHBoxLayout()
        tx_vol_row.addWidget(QLabel("Outgoing Gain:"))
        self.tx_vol_slider = QSlider(Qt.Orientation.Horizontal)
        self.tx_vol_slider.setRange(0, 150)
        self.tx_vol_slider.setValue(100)
        self.tx_vol_slider.valueChanged.connect(self.on_tx_vol_changed)
        tx_vol_row.addWidget(self.tx_vol_slider, 1)
        self.tx_vol_lbl = QLabel("100%")
        self.tx_vol_lbl.setFixedWidth(42)
        tx_vol_row.addWidget(self.tx_vol_lbl)
        v_audio.addLayout(tx_vol_row)

        test_chime_btn = QPushButton("🔔 Play Test Chime (Verify PipeWire + UDP Audio Bus)")
        test_chime_btn.clicked.connect(lambda: self.post_action("test_audio_chime"))
        v_audio.addWidget(test_chime_btn)

        sidebar.addWidget(card_audio)

        # --- CARD 4: GAME CAPTIVE LOCK ---
        card_game = QFrame()
        card_game.setObjectName("card")
        v_game = QVBoxLayout(card_game)
        v_game.setContentsMargins(14, 12, 14, 12)
        v_game.setSpacing(8)

        lbl_c4 = QLabel("4. GAME CAPTIVE LOCK (KEEP MOUSE IN GAMES)")
        lbl_c4.setObjectName("sectionTitle")
        v_game.addWidget(lbl_c4)

        self.auto_game_chk = QCheckBox("Auto-Lock when Fullscreen / Steam / 3D Game runs")
        self.auto_game_chk.setChecked(True)
        self.auto_game_chk.toggled.connect(lambda _v: self.schedule_push_layout())
        v_game.addWidget(self.auto_game_chk)

        self.manual_game_btn = QPushButton("🔒 Lock Mouse to Current Screen (Ctrl+Alt+G)")
        self.manual_game_btn.clicked.connect(lambda: self.post_action("toggle_game_lock"))
        v_game.addWidget(self.manual_game_btn)

        sidebar.addWidget(card_game)
        sidebar.addStretch(1)

        # Poll daemon state at 60ms intervals
        self.poll_timer = QTimer(self)
        self.poll_timer.timeout.connect(self.fetch_state)
        self.poll_timer.start(60)
        self.fetch_state(initial=True)

    def snap_windows_box(self, wx, wy, ww, wh, monitors):
        best = None
        best_dist = 1e18
        total_w = max((m["x"] + m["w"] for m in monitors), default=4480)

        for m in monitors:
            mx, my, mw, mh = m["x"], m["y"], m["w"], m["h"]

            if mx + mw >= total_w:
                tx = mx + mw
                cy = max(my - wh + 240, min(wy, my + mh - 240))
                d = math.hypot(wx - tx, wy - cy)
                if d < best_dist:
                    best_dist = d
                    best = (int(round(tx)), int(round(cy)), "Right")

            if mx <= 0:
                tx = mx - ww
                cy = max(my - wh + 240, min(wy, my + mh - 240))
                d = math.hypot(wx - tx, wy - cy)
                if d < best_dist:
                    best_dist = d
                    best = (int(round(tx)), int(round(cy)), "Left")

            ty = my + mh
            cx = max(mx - ww + 280, min(wx, mx + mw - 280))
            d = math.hypot(wx - cx, wy - ty)
            if d < best_dist:
                best_dist = d
                best = (int(round(cx)), int(round(ty)), "Bottom")

            ty = my - wh
            cx = max(mx - ww + 280, min(wx, mx + mw - 280))
            d = math.hypot(wx - cx, wy - ty)
            if d < best_dist:
                best_dist = d
                best = (int(round(cx)), int(round(ty)), "Top")

        return best

    def fetch_state(self, initial=False):
        try:
            req = urllib.request.urlopen(f"{API_BASE}/api/state", timeout=0.2)
            data = json.loads(req.read().decode("utf-8"))
            if not self.canvas.dragging_win and not self.push_timer.isActive():
                self.state = data
                if initial:
                    self.sync_controls_from_state()
            elif self.state:
                self.state["telemetry"] = data["telemetry"]
            self.update_status_labels()
            self.canvas.update()
        except Exception:
            pass

    def sync_controls_from_state(self):
        if not self.state:
            return
        self.updating_ui_programmatically = True
        try:
            c = self.state["config"]
            t = self.state["telemetry"]

            spd_pct = int(round(float(c.get("mouse_speed", 0.35)) * 100))
            self.speed_slider.setValue(max(5, min(150, spd_pct)))
            self.speed_val_lbl.setText(f"{spd_pct / 100.0:.2f}× ({spd_pct}%)")

            resist = int(c.get("edge_resistance", 55))
            self.resist_slider.setValue(max(10, min(180, resist)))
            self.resist_val_lbl.setText(f"{resist} px")

            self.auto_game_chk.setChecked(bool(c.get("auto_game_lock", True)))

            mode = c.get("resolution_mode", "native")
            res_str = f"{c['win_w']}x{c['win_h']}"
            target_val = "native" if mode == "native" else res_str
            for i in range(self.res_combo.count()):
                if self.res_combo.itemData(i) == target_val:
                    self.res_combo.setCurrentIndex(i)
                    break

            self.rx_vol_slider.setValue(int(t.get("audio_volume", 100)))
            self.tx_vol_slider.setValue(int(t.get("audio_tx_volume", 100)))
            self.tx_source_combo.setCurrentIndex(int(t.get("audio_tx_source_mode", 0)))
        finally:
            self.updating_ui_programmatically = False

    def update_status_labels(self):
        if not self.state:
            return
        c = self.state["config"]
        t = self.state["telemetry"]

        # Debian native monitors summary
        deb_parts = [f"{m['name'].split(' ')[0]} ({m['w']}×{m['h']})" for m in c["debian_monitors"]]
        win_native = f"{c.get('win_native_w', 1920)}×{c.get('win_native_h', 1080)}"
        self.deb_native_lbl.setText(
            f"Native Hardware: {', '.join(deb_parts)}  |  Win11 Native: {win_native}"
        )

        # Update first combo item text with live detected native resolution
        self.res_combo.setItemText(0, f"Native Auto-Detect ({win_native})")

        self.border_readout_lbl.setText(
            f"Attached Border: {c['attach_side']} Edge  |  Coords: ({c['win_x']}, {c['win_y']})  |  Size: {c['win_w']}×{c['win_h']}"
        )

        node = t.get("active_node", "Debian")
        peer_online = bool(t.get("peer_online", False))
        if peer_online:
            self.peer_badge.setText("🟢 Laptop Online (Mesh Active)")
            self.peer_badge.setStyleSheet(
                "background-color: rgba(16, 185, 129, 0.16); color: #6ee7b7; border: 1px solid #059669; border-radius: 12px; padding: 4px 12px; font-weight: 600;"
            )
        else:
            self.peer_badge.setText("⚪ Laptop Away / Offline (Standalone Mode)")
            self.peer_badge.setStyleSheet(
                "background-color: rgba(51, 65, 85, 0.5); color: #94a3b8; border: 1px solid #475569; border-radius: 12px; padding: 4px 12px; font-weight: 600;"
            )

        if node == "Debian":
            self.active_badge.setText("Active Focus: 🖥 Debian Desktop")
            self.active_badge.setStyleSheet(
                "background-color: rgba(16, 185, 129, 0.22); color: #34d399; border: 1px solid #059669; border-radius: 12px; padding: 4px 12px; font-weight: 700;"
            )
        else:
            self.active_badge.setText("Active Focus: 💻 Windows 11 Laptop")
            self.active_badge.setStyleSheet(
                "background-color: rgba(99, 102, 241, 0.28); color: #a5b4fc; border: 1px solid #6366f1; border-radius: 12px; padding: 4px 12px; font-weight: 700;"
            )

        game_locked = bool(t.get("game_captive_active", False))
        reason = t.get("game_reason", "")
        if game_locked:
            self.game_badge.setText(f"🔒 GAME CAPTIVE LOCKED ({reason})")
            self.game_badge.setStyleSheet(
                "background-color: rgba(245, 158, 11, 0.25); color: #fcd34d; border: 1px solid #f59e0b; border-radius: 12px; padding: 4px 12px; font-weight: 700;"
            )
            self.manual_game_btn.setText("🔓 Unlock Game Captive Mode (Ctrl+Alt+G)")
            self.manual_game_btn.setObjectName("activeToggleBtn")
        else:
            self.game_badge.setText("🎮 Game Lock: Auto-Standby")
            self.game_badge.setStyleSheet(
                "background-color: rgba(51, 65, 85, 0.5); color: #cbd5e1; border: 1px solid #475569; border-radius: 12px; padding: 4px 12px; font-weight: 600;"
            )
            self.manual_game_btn.setText("🔒 Lock Mouse to Current Screen (Ctrl+Alt+G)")
            self.manual_game_btn.setObjectName("")

        cx = t["deb_cursor_x"] if node == "Debian" else t["win_cursor_x"]
        cy = t["deb_cursor_y"] if node == "Debian" else t["win_cursor_y"]
        self.cursor_readout.setText(f"{node} Cursor: ({cx}, {cy})")

        # Audio telemetry
        rx_pkts = t.get("audio_rx_packets", 0)
        rx_peak = int(t.get("audio_rx_peak", 0))
        rx_muted = bool(t.get("audio_muted", False))
        tx_on = bool(t.get("audio_tx_enabled", False))
        tx_pkts = t.get("audio_tx_packets", 0)
        tx_peak = int(t.get("audio_tx_peak", 0))

        self.rx_vu_bar.setValue(0 if rx_muted else rx_peak)
        self.tx_vu_bar.setValue(tx_peak)

        if rx_muted:
            self.rx_mute_btn.setText("🔇 Muted (Click to Unmute)")
        else:
            self.rx_mute_btn.setText("🔊 Output Active")

        if rx_pkts > 0:
            self.rx_status_lbl.setText(
                f"Streaming Live from Windows ({rx_pkts:,} packets received) -> PipeWire"
            )
        else:
            self.rx_status_lbl.setText(
                "Ready on UDP 24851 -> PipeWire 48kHz Stereo (Waiting for Windows audio stream)"
            )

        if tx_on:
            self.tx_toggle_btn.setText(
                f"🟢 Share Debian Audio to Windows: ON ({tx_pkts:,} pkts sent)"
            )
            self.tx_toggle_btn.setStyleSheet(
                "background-color: #059669; border: 1px solid #34d399; color: #ffffff; font-weight: 700; padding: 8px;"
            )
        else:
            self.tx_toggle_btn.setText("⚪ Share Debian Audio to Windows: OFF (Click to Enable)")
            self.tx_toggle_btn.setStyleSheet("")

    def on_res_combo_changed(self, idx):
        if self.updating_ui_programmatically or not self.state or idx < 0:
            return
        val = self.res_combo.itemData(idx)
        c = self.state["config"]
        if val == "native":
            c["resolution_mode"] = "native"
            c["win_w"] = int(c.get("win_native_w", 1920))
            c["win_h"] = int(c.get("win_native_h", 1080))
        else:
            c["resolution_mode"] = "custom"
            w, h = [int(x) for x in val.split("x")]
            c["win_w"] = w
            c["win_h"] = h

        snapped = self.snap_windows_box(
            c["win_x"], c["win_y"], c["win_w"], c["win_h"], c["debian_monitors"]
        )
        if snapped:
            c["win_x"], c["win_y"], c["attach_side"] = snapped

        self.update_status_labels()
        self.canvas.update()
        self.push_layout(
            status_msg=f"✓ Display resolution updated to {c['win_w']}×{c['win_h']} ({c['resolution_mode']} mode)!"
        )

    def on_reset_native_clicked(self):
        self.post_action("reset_native_resolutions")
        self.updating_ui_programmatically = True
        self.res_combo.setCurrentIndex(0)
        self.updating_ui_programmatically = False
        self.status_bar_lbl.setText(
            "✓ All Debian & Windows displays reset to native hardware resolutions!"
        )
        QTimer.singleShot(100, lambda: self.fetch_state(initial=True))

    def apply_preset(self, code):
        if not self.state:
            return
        c = self.state["config"]
        ww, wh = c["win_w"], c["win_h"]
        if code == "below_dp2":
            c["win_x"] = int(1920 + (2560 - ww) // 2)
            c["win_y"] = 1440
            c["attach_side"] = "Bottom"
        elif code == "right_dp2":
            c["win_x"] = 4480
            c["win_y"] = int((1440 - wh) // 2)
            c["attach_side"] = "Right"
        elif code == "below_dp3":
            c["win_x"] = int((1920 - ww) // 2)
            c["win_y"] = 1440
            c["attach_side"] = "Bottom"
        elif code == "left_dp3":
            c["win_x"] = -ww
            c["win_y"] = 360
            c["attach_side"] = "Left"
        elif code == "above_dp2":
            c["win_x"] = int(1920 + (2560 - ww) // 2)
            c["win_y"] = -wh
            c["attach_side"] = "Top"
        elif code == "above_dp3":
            c["win_x"] = 0
            c["win_y"] = 360 - wh
            c["attach_side"] = "Top"

        self.update_status_labels()
        self.canvas.update()
        self.push_layout(
            status_msg=f"✓ Snapped Windows 11 Laptop to {c['attach_side']} border at ({c['win_x']}, {c['win_y']})!"
        )

    def on_speed_slider_changed(self, val):
        spd = val / 100.0
        self.speed_val_lbl.setText(f"{spd:.2f}× ({val}%)")
        if not self.updating_ui_programmatically and self.state:
            self.state["config"]["mouse_speed"] = round(spd, 2)
            self.schedule_push_layout()

    def on_resist_slider_changed(self, val):
        self.resist_val_lbl.setText(f"{val} px")
        if not self.updating_ui_programmatically and self.state:
            self.state["config"]["edge_resistance"] = int(val)
            self.schedule_push_layout()

    def on_rx_vol_changed(self, val):
        self.rx_vol_lbl.setText(f"{val}%")
        if not self.updating_ui_programmatically:
            self.post_json("/api/action", {"action": "set_audio_volume", "volume": int(val)})

    def on_tx_vol_changed(self, val):
        self.tx_vol_lbl.setText(f"{val}%")
        if not self.updating_ui_programmatically:
            self.post_json("/api/action", {"action": "set_audio_tx_volume", "volume": int(val)})

    def on_tx_source_changed(self, idx):
        if not self.updating_ui_programmatically and idx >= 0:
            mode = int(self.tx_source_combo.itemData(idx))
            self.post_json("/api/action", {"action": "set_audio_tx_source", "mode": mode})
            src_name = "Desktop System Audio Loopback" if mode == 0 else "Microphone Input"
            self.status_bar_lbl.setText(f"✓ Switched Debian audio capture source to: {src_name}")

    def schedule_push_layout(self):
        self.push_timer.start(45)

    def _do_push_layout(self):
        self.push_layout()

    def push_layout(self, status_msg=None):
        if not self.state:
            return
        c = self.state["config"]
        c["mouse_speed"] = round(self.speed_slider.value() / 100.0, 2)
        c["edge_resistance"] = int(self.resist_slider.value())
        c["auto_game_lock"] = bool(self.auto_game_chk.isChecked())
        if self.post_json("/api/layout", c):
            if status_msg:
                self.status_bar_lbl.setText(status_msg)
            else:
                self.status_bar_lbl.setText(
                    f"✓ Applied live! Mouse Speed: {c['mouse_speed']:.2f}×  |  Border: {c['attach_side']} ({c['win_x']}, {c['win_y']})  |  Res: {c['win_w']}×{c['win_h']}"
                )

    def post_action(self, action_name):
        self.post_json("/api/action", {"action": action_name})
        QTimer.singleShot(50, self.fetch_state)

    def post_json(self, path, payload):
        try:
            body = json.dumps(payload).encode("utf-8")
            req = urllib.request.Request(
                f"{API_BASE}{path}",
                data=body,
                headers={"Content-Type": "application/json"},
                method="POST",
            )
            urllib.request.urlopen(req, timeout=0.35)
            return True
        except Exception as e:
            self.status_bar_lbl.setText(f"⚠ Connection warning: {e}")
            return False


def main():
    app = QApplication(sys.argv)
    app.setStyleSheet(DARK_STYLESHEET)
    win = MeshKvmStudio()
    win.show()
    sys.exit(app.exec())


if __name__ == "__main__":
    main()
