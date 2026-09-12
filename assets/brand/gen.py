"""Vorcall brand generator: the creature mark, its app-icon tile and the two animations.

Run:  uvx --with shapely python3 assets/brand/gen.py assets/brand
(shapely is only used to union the projected slab silhouettes for the entrance.)
Everything is derived from the shapes below; edit them and rerun rather than editing the SVGs.
"""
import math
import os
import sys

RED = "#C8102E"  # "Deep"
HAND = "#8F0B22"       # darker Deep, the creases between the fingers in the rare entrance
STEEL = "#454A52"      # laptop lid
DECK_LIGHT = "#6A717B" # laptop deck
KEYS = "#363B42"       # keyboard

# ----------------------------------------------------------------------------
# Static mark: hand-written arcs, one path, eyes wound the other way (holes).
# ----------------------------------------------------------------------------

def ellipse(cx, cy, rx, ry, ccw=True):
    s = 0 if ccw else 1
    return f"M{cx - rx} {cy} A{rx} {ry} 0 1 {s} {cx + rx} {cy} A{rx} {ry} 0 1 {s} {cx - rx} {cy} Z"


def foot(cx, top, w=30, bottom=226, r=12):
    l, rt = cx - w / 2, cx + w / 2
    return (f"M{l} {top} L{rt} {top} L{rt} {bottom - r} A{r} {r} 0 0 1 {rt - r} {bottom} "
            f"L{l + r} {bottom} A{r} {r} 0 0 1 {l} {bottom - r} Z")


def arm_left(x_out, x_in, top, bottom, r=11):
    return (f"M{x_out + r} {top} L{x_in} {top} L{x_in} {bottom} L{x_out + r} {bottom} "
            f"A{r} {r} 0 0 1 {x_out} {bottom - r} L{x_out} {top + r} A{r} {r} 0 0 1 {x_out + r} {top} Z")


def arm_right(x_out, x_in, top, bottom, r=11):
    return (f"M{x_in} {top} L{x_out - r} {top} A{r} {r} 0 0 1 {x_out} {top + r} L{x_out} {bottom - r} "
            f"A{r} {r} 0 0 1 {x_out - r} {bottom} L{x_in} {bottom} Z")


def horn_points(base, ctrl, tip, w0=36, w_tip=14, taper=1.0, n=24, tip_steps=8, root=0.0):
    """Outline of a tapered horn along a quadratic curve, as a clockwise polygon (round tip sampled).
    `root` starts the outline part-way up the curve (0 = at `base`); the width taper is unchanged."""
    def P(t):
        u = 1 - t
        return (u * u * base[0] + 2 * u * t * ctrl[0] + t * t * tip[0],
                u * u * base[1] + 2 * u * t * ctrl[1] + t * t * tip[1])

    def D(t):
        u = 1 - t
        return (2 * u * (ctrl[0] - base[0]) + 2 * t * (tip[0] - ctrl[0]),
                2 * u * (ctrl[1] - base[1]) + 2 * t * (tip[1] - ctrl[1]))

    left, right = [], []
    for i in range(n + 1):
        t = root + (1 - root) * i / n
        x, y = P(t)
        dx, dy = D(t)
        L = (dx * dx + dy * dy) ** 0.5
        nx, ny = -dy / L, dx / L
        w = (w0 - w_tip) * (1 - t) ** taper + w_tip
        left.append((x - nx * w / 2, y - ny * w / 2))
        right.append((x + nx * w / 2, y + ny * w / 2))
    r = w_tip / 2
    a0 = math.atan2(left[-1][1] - tip[1], left[-1][0] - tip[0])
    cap = [(tip[0] + r * math.cos(a0 + k * math.pi / tip_steps), tip[1] + r * math.sin(a0 + k * math.pi / tip_steps))
           for k in range(1, tip_steps)]
    return left + cap + right[::-1]


def horn(base, ctrl, tip, **kw):
    pts = horn_points(base, ctrl, tip, **kw)
    return "M" + " L".join(f"{x:.1f} {y:.1f}" for x, y in pts) + " Z"


def mirror(pt):
    return (256 - pt[0], pt[1])


BODY = ("M96 74 L160 74 A52 52 0 0 1 212 126 L212 168 A34 34 0 0 1 178 202 "
        "L78 202 A34 34 0 0 1 44 168 L44 126 A52 52 0 0 1 96 74 Z")
HORN_ARGS = ((82, 112), (86, 62), (48, 36))
HORN_L = horn(*HORN_ARGS)
HORN_R = horn(*[mirror(p) for p in HORN_ARGS])
ARM_L = arm_left(26, 60, 132, 162)
ARM_R = arm_right(230, 196, 132, 162)
FOOT_L = foot(84, 176)
FOOT_R = foot(172, 176)
EYE_L = (100, 130, 10, 16)
EYE_R = (156, 130, 10, 16)

MARK_D = " ".join([BODY, HORN_L, HORN_R, FOOT_L, FOOT_R, ARM_L, ARM_R, ellipse(*EYE_L), ellipse(*EYE_R)])


def mark_svg():
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-label="Vorcall">\n'
            f'  <path fill="{RED}" fill-rule="nonzero" d="{MARK_D}"/>\n</svg>\n')


def icon_svg():
    return (f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-label="Vorcall">\n'
            f'  <rect width="256" height="256" rx="56" fill="{RED}"/>\n'
            f'  <path fill="#FFFFFF" fill-rule="nonzero" transform="translate(128 128) scale(0.74) translate(-128 -128)" d="{MARK_D}"/>\n</svg>\n')


# ----------------------------------------------------------------------------
# Rig for the entrance: the same creature as cubic-bezier segments so every point
# can be perspective-projected, and the path morphed between poses with SMIL.
# ----------------------------------------------------------------------------

def carc(cx, cy, rx, ry, a1, a2):
    """Cubic approximation of an elliptical arc from angle a1 to a2 (degrees, span <= 90, screen coords)."""
    a1, a2 = math.radians(a1), math.radians(a2)
    k = 4 / 3 * math.tan((a2 - a1) / 4)
    p0 = (cx + rx * math.cos(a1), cy + ry * math.sin(a1))
    p3 = (cx + rx * math.cos(a2), cy + ry * math.sin(a2))
    p1 = (p0[0] - k * rx * math.sin(a1), p0[1] + k * ry * math.cos(a1))
    p2 = (p3[0] + k * rx * math.sin(a2), p3[1] - k * ry * math.cos(a2))
    return ("C", p1, p2, p3)


def body_sub():
    return [("M", (96, 74)), ("L", (160, 74)), carc(160, 126, 52, 52, -90, 0), ("L", (212, 168)),
            carc(178, 168, 34, 34, 0, 90), ("L", (78, 202)), carc(78, 168, 34, 34, 90, 180),
            ("L", (44, 126)), carc(96, 126, 52, 52, 180, 270)]


def foot_sub(cx, top=176, w=30, bottom=226, r=12):
    l, rt = cx - w / 2, cx + w / 2
    return [("M", (l, top)), ("L", (rt, top)), ("L", (rt, bottom - r)), carc(rt - r, bottom - r, r, r, 0, 90),
            ("L", (l + r, bottom)), carc(l + r, bottom - r, r, r, 90, 180)]


def arm_left_sub(x_out=26, x_in=60, top=132, bottom=162, r=11):
    return [("M", (x_out + r, top)), ("L", (x_in, top)), ("L", (x_in, bottom)), ("L", (x_out + r, bottom)),
            carc(x_out + r, bottom - r, r, r, 90, 180), ("L", (x_out, top + r)), carc(x_out + r, top + r, r, r, 180, 270)]


def arm_right_sub(x_out=230, x_in=196, top=132, bottom=162, r=11):
    return [("M", (x_in, top)), ("L", (x_out - r, top)), carc(x_out - r, top + r, r, r, 270, 360),
            ("L", (x_out, bottom - r)), carc(x_out - r, bottom - r, r, r, 0, 90), ("L", (x_in, bottom))]


def horn_sub(pts):
    return [("M", pts[0])] + [("L", p) for p in pts[1:]]


def eye_sub(cx, cy, rx, ry):
    return [("M", (cx + rx, cy)), carc(cx, cy, rx, ry, 0, 90), carc(cx, cy, rx, ry, 90, 180),
            carc(cx, cy, rx, ry, 180, 270), carc(cx, cy, rx, ry, 270, 360)]


# The rig's horns stop short of the eye line. The client fills every ring of a frame in one
# nonzero pass, so wherever two solids sit under an eye the winding is 2 - 1 and the hole
# fills in; the swept horn base reaches the popped near eye once the creature has turned.
# Trimmed here, the root still ends well inside the body (the visible horn is unchanged).
HORN_ROOT = 0.12

RIG = {
    "body": body_sub(),
    "horn_l": horn_sub(horn_points(*HORN_ARGS, n=16, tip_steps=6, root=HORN_ROOT)),
    "horn_r": horn_sub(horn_points(*[mirror(p) for p in HORN_ARGS], n=16, tip_steps=6, root=HORN_ROOT)),
    "foot_l": foot_sub(84),
    "foot_r": foot_sub(172),
    "arm_l": arm_left_sub(),
    "arm_r": arm_right_sub(),
    "eye_l": eye_sub(*EYE_L),   # near eye: the creature turns its left side to the camera
    "eye_r": eye_sub(*EYE_R),
}
SOLID_PARTS = ["body", "horn_l", "horn_r", "foot_l", "foot_r", "arm_l", "arm_r"]
# Thickness: every part is a slab whose front face sits on z = 0 (so the front view is the
# logo) and extends back by DEPTH. The silhouette of the yawed slab is the union of the front
# outline, the back outline and, per outline edge, the projected quad it sweeps: exact, because
# straight lines stay straight under perspective.
DEPTH = {"body": 44, "horn_l": 12, "horn_r": 12, "foot_l": 22, "foot_r": 22, "arm_l": 22, "arm_r": 22}
POINTS = {"body": 120, "horn_l": 64, "horn_r": 64, "foot_l": 40, "foot_r": 40, "arm_l": 36, "arm_r": 36,
          "fist": 72, "finger": 28, "thumb": 40, "crease0": 12, "crease1": 12, "crease2": 12}
HAND_PARTS = ["fist", "finger", "thumb"]            # the rare gesture, on the end of the raised near arm
SHADED_PARTS = ["crease0", "crease1", "crease2"]    # thin dark creases between the fingers, drawn on top
RARE_PARTS = SOLID_PARTS + HAND_PARTS

FOCAL = 200       # camera distance in viewBox units; smaller = stronger perspective
PIVOT_Y = 138     # eye line of the camera, in viewBox y
GROUND = (128, 226)


class Pose:
    def __init__(self, theta=0, z=40, axis=0, foot_lift=0, foot_z=0, lift=0, lean=0, eye_pop=1.0, wink=1.0,
                 arm_raise=0, arm_stretch=0, finger=0, arm_depth=22):
        self.theta = theta        # yaw in degrees; positive brings the left side toward the camera
        self.z = z                # depth offset of the whole creature; negative = closer
        self.axis = axis          # x of the yaw axis relative to centre; positive = pivots nearer the far side
        self.foot_lift = foot_lift
        self.foot_z = foot_z      # extra depth for the stepping foot; negative = closer
        self.lift = lift          # body bob, negative = up
        self.lean = lean          # in-plane lean about the ground point, degrees
        self.eye_pop = eye_pop    # cartoon exaggeration of the near eye
        self.wink = wink          # 1 open .. ~0 closed, near eye only
        self.arm_raise = arm_raise      # near arm swung up and out, degrees from resting (rare variant)
        self.arm_stretch = arm_stretch  # cartoon stretch of the raised arm, fraction of its length
        self.finger = finger            # 0 none .. 1 fully extended
        self.arm_depth = arm_depth      # slab depth of the near arm and hand; thinner while gesturing

    def with_(self, **kw):
        p = Pose(**self.__dict__)
        p.__dict__.update(kw)
        return p


def world(pose, part, pt, zi=0.0):
    """Model point (with creature-local depth zi) -> world (x, y, z) in front of the camera."""
    x, y = pt
    if part == "foot_l":
        y += pose.foot_lift
    t = math.radians(pose.theta)
    if part == "eye_l":
        # near eye: faces the lens instead of lying flat on the cutout, so undo the yaw's
        # foreshortening on its width; pop it a little; the wink squashes it vertically
        cx, cy = EYE_L[0], EYE_L[1]
        x = cx + (x - cx) * pose.eye_pop / math.cos(t)
        y = cy + (y - cy) * pose.eye_pop * pose.wink
    if part == "eye_r":
        # far eye: turned away, so it foreshortens harder than the cutout and shrinks a touch
        cx, cy = EYE_R[0], EYE_R[1]
        s = 1 - 0.1 * pose.theta / 34
        x = cx + (x - cx) * s * (1 - 0.2 * pose.theta / 34)
        y = cy + (y - cy) * s
    # bob and lean the whole creature
    a = math.radians(pose.lean)
    gx, gy = GROUND
    x, y = x - gx, y - gy
    x, y = x * math.cos(a) - y * math.sin(a), x * math.sin(a) + y * math.cos(a)
    x, y = x + gx, y + gy + pose.lift
    # yaw about a vertical axis through the body's mid-depth
    xp, yp = x - 128, y - PIVOT_Y
    half = DEPTH["body"] / 2
    rel_x, rel_z = xp - pose.axis, zi - half
    xr = pose.axis + rel_x * math.cos(t) - rel_z * math.sin(t)
    z = half + rel_x * math.sin(t) + rel_z * math.cos(t) + pose.z
    if part == "foot_l":
        z += pose.foot_z
    return (xr, yp, z)


def persp(w):
    k = FOCAL / (FOCAL + w[2])
    return (w[0] * k, w[1] * k)


def project(pose, part, pt, zi=0.0):
    return persp(world(pose, part, pt, zi))


def flatten(sub, steps=8):
    """Outline commands -> polygon points (cubics sampled), without a closing duplicate."""
    pts, cur = [], None
    for cmd in sub:
        if cmd[0] in ("M", "L"):
            cur = cmd[1]
            pts.append(cur)
        else:
            p0, p1, p2, p3 = cur, cmd[1], cmd[2], cmd[3]
            for s in range(1, steps + 1):
                t, u = s / steps, 1 - s / steps
                pts.append((u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0],
                            u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1]))
            cur = p3
    if math.dist(pts[0], pts[-1]) < 1e-6:
        pts.pop()
    return pts


OUTLINES = {part: flatten(RIG[part]) for part in SOLID_PARTS}


def arm_left_gesture(raise_deg=0.0, stretch=0.0, shoulder=(60, 147), length=34, half=15, r=11, steps=6):
    """Near arm outline: a rounded bar from the shoulder, swung up by raise_deg and stretched.
    With both at zero it is exactly the resting arm."""
    a = math.radians(raise_deg)
    d = (-math.cos(a), -math.sin(a))
    n = (-d[1], d[0])
    L = length * (1 + stretch)

    def P(u, v):
        return (shoulder[0] + d[0] * u + n[0] * v, shoulder[1] + d[1] * u + n[1] * v)

    pts = [P(0, half), P(L - r, half)]
    for k in range(1, steps + 1):
        ang = math.pi / 2 * k / steps
        pts.append(P(L - r + r * math.sin(ang), half - r + r * math.cos(ang)))
    pts.append(P(L, -(half - r)))
    for k in range(1, steps + 1):
        ang = math.pi / 2 * k / steps
        pts.append(P(L - r + r * math.cos(ang), -(half - r) - r * math.sin(ang)))
    pts.append(P(0, -half))
    return pts


def fist_centre(pose, shoulder=(60, 147), length=34, half=15):
    a = math.radians(pose.arm_raise)
    L = length * (1 + pose.arm_stretch)
    return (shoulder[0] - math.cos(a) * (L - half), shoulder[1] - math.sin(a) * (L - half))


# The hand, seen from the back with the fingers up, in local units around the fist centre.
# Four finger slots across the top: index, middle (raised), ring, pinky; thumb curls out of the
# outer side. Everything scales in with the finger so the resting arm stays a plain nub.
FIST_W, FIST_TOP, FIST_BOTTOM, SLOT = 34, -12, 14, 8.5


def hand_scale(pose):
    return min(1.0, max(pose.finger, 0.02) / 0.6)


def hand_points(pose, local):
    cx, cy = fist_centre(pose)
    s = hand_scale(pose)
    return [(cx + u * s, cy + v * s) for u, v in local]


def fist_outline(pose, r=8, steps=6):
    """Rounded block with four knuckle bumps along the top."""
    h = FIST_W / 2
    pts = []
    for i in range(4):
        kx = -h + SLOT * (i + 0.5)
        for k in range(steps + 1):
            ang = math.pi * k / steps
            pts.append((kx - SLOT / 2 * math.cos(ang), FIST_TOP - SLOT / 2 * math.sin(ang)))
    pts.append((h, FIST_BOTTOM - r))
    for k in range(1, steps + 1):
        ang = math.pi / 2 * k / steps
        pts.append((h - r + r * math.cos(ang), FIST_BOTTOM - r + r * math.sin(ang)))
    pts.append((-h + r, FIST_BOTTOM))
    for k in range(1, steps + 1):
        ang = math.pi / 2 * k / steps
        pts.append((-h + r - r * math.sin(ang), FIST_BOTTOM - r + r * math.cos(ang)))
    return hand_points(pose, pts)


def finger_outline(pose, fw=5, flen=32, steps=8):
    """The middle finger, rising from the second slot; its length follows pose.finger."""
    f = max(pose.finger, 0.02)
    w = fw * min(1.0, f / 0.25)
    F = flen * f
    x0 = -FIST_W / 2 + SLOT * 1.5
    base = FIST_TOP + 3
    pts = [(x0 - w, base), (x0 - w, base - F + w)]
    for k in range(1, steps):
        ang = math.pi * k / steps
        pts.append((x0 - w * math.cos(ang), base - F + w - w * math.sin(ang)))
    pts += [(x0 + w, base - F + w), (x0 + w, base)]
    cx, cy = fist_centre(pose)
    return [(cx + u, cy + v) for u, v in pts]


def capsule(p, q, w, steps=8):
    """Polygon of a rounded bar from p to q with full width w."""
    dx, dy = q[0] - p[0], q[1] - p[1]
    L = math.hypot(dx, dy)
    ux, uy = dx / L, dy / L
    r = w / 2
    a0 = math.atan2(uy, ux)
    pts = []
    for k in range(steps + 1):
        ang = a0 + math.pi / 2 + math.pi * k / steps
        pts.append((p[0] + r * math.cos(ang), p[1] + r * math.sin(ang)))
    for k in range(steps + 1):
        ang = a0 - math.pi / 2 + math.pi * k / steps
        pts.append((q[0] + r * math.cos(ang), q[1] + r * math.sin(ang)))
    return pts


def thumb_outline(pose, base=(-12, 5), knuckle=(-21, 3.5), tip=(-25, -3)):
    """The thumb: straight out of the outer side of the fist, then the last joint bent back up."""
    from shapely.geometry import Polygon
    from shapely.geometry.polygon import orient
    from shapely.ops import unary_union
    shape = unary_union([Polygon(capsule(base, knuckle, 9)), Polygon(capsule(knuckle, tip, 7.5))])
    return hand_points(pose, list(orient(shape, 1.0).exterior.coords)[:-1])


def crease_outline(pose, i, w=1.3, top=FIST_TOP - 1, length=8, steps=4):
    """A thin rounded dark line down from a valley between two knuckles."""
    x = -FIST_W / 2 + SLOT * (i + 1)
    pts = [(x - w, top), (x + w, top), (x + w, top + length - w)]
    for k in range(1, steps):
        ang = math.pi * k / steps
        pts.append((x + w * math.cos(ang), top + length - w + w * math.sin(ang)))
    pts.append((x - w, top + length - w))
    return hand_points(pose, pts)


def outline_for(pose, part):
    if part == "arm_l":
        return arm_left_gesture(pose.arm_raise, pose.arm_stretch)
    if part == "fist":
        return fist_outline(pose)
    if part == "finger":
        return finger_outline(pose)
    if part == "thumb":
        return thumb_outline(pose)
    if part.startswith("crease"):
        return crease_outline(pose, int(part[-1]))
    return OUTLINES[part]


def depth_for(pose, part):
    if part in SHADED_PARTS:
        return 0.0
    return pose.arm_depth if part == "arm_l" or part in HAND_PARTS else DEPTH[part]


def sweep_silhouette(front, back, n_points):
    from shapely.geometry import MultiPoint, Polygon
    from shapely.geometry.polygon import orient
    from shapely.ops import unary_union
    pieces = [Polygon(front).buffer(0), Polygon(back).buffer(0)]
    n = len(front)
    for i in range(n):
        j = (i + 1) % n
        hull = MultiPoint([front[i], front[j], back[j], back[i]]).convex_hull
        if hull.geom_type == "Polygon":
            pieces.append(hull)
    union = unary_union(pieces)
    if union.geom_type == "MultiPolygon":
        union = max(union.geoms, key=lambda g: g.area)
    ring = list(orient(union, 1.0).exterior.coords)
    return resample(ring, n_points, front[0])


def slab_silhouette(pose, part):
    pts = outline_for(pose, part)
    front = [project(pose, part, p, 0.0) for p in pts]
    back = [project(pose, part, p, depth_for(pose, part)) for p in pts]
    return sweep_silhouette(front, back, POINTS[part])


def resample(ring, n, anchor):
    """Evenly spaced points along a closed ring, starting nearest the anchor (stable morph targets)."""
    ring = ring[:-1]
    k = min(range(len(ring)), key=lambda i: math.dist(ring[i], anchor))
    ring = ring[k:] + ring[:k]
    ring.append(ring[0])
    cum = [0.0]
    for a, b in zip(ring, ring[1:]):
        cum.append(cum[-1] + math.dist(a, b))
    total, out, seg = cum[-1], [], 0
    for j in range(n):
        s = total * j / n
        while cum[seg + 1] < s:
            seg += 1
        a, b = ring[seg], ring[seg + 1]
        f = (s - cum[seg]) / (cum[seg + 1] - cum[seg]) if cum[seg + 1] > cum[seg] else 0
        out.append((a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f))
    return out


def project_rig(pose, parts=SOLID_PARTS, centre_on=None):
    """Solid parts as swept-slab polygons, eyes as projected cubics; everything recentred on the
    bbox of `centre_on` (default: every solid part)."""
    solids = {part: slab_silhouette(pose, part) for part in parts}
    eyes = {}
    for part in ("eye_l", "eye_r"):
        eyes[part] = [(cmd[0],) + tuple(project(pose, part, p) for p in cmd[1:]) for cmd in RIG[part]]
    xs = [p[0] for part, poly in solids.items() if part in (centre_on or parts) for p in poly]
    dx = 128 - (min(xs) + max(xs)) / 2
    out = {}
    for part, poly in solids.items():
        out[part] = [("M", (poly[0][0] + dx, poly[0][1] + PIVOT_Y))] + [("L", (p[0] + dx, p[1] + PIVOT_Y)) for p in poly[1:]]
    for part, sub in eyes.items():
        out[part] = [(cmd[0],) + tuple((p[0] + dx, p[1] + PIVOT_Y) for p in cmd[1:]) for cmd in sub]
    return out


def d_of(subs):
    parts = []
    for sub in subs:
        parts.append(" ".join(cmd[0] + " ".join(f"{p[0]:.1f} {p[1]:.1f}" for p in cmd[1:]) for cmd in sub) + " Z")
    return " ".join(parts)


def bbox(subs):
    pts = [p for sub in subs for cmd in sub for p in cmd[1:]]
    return (min(p[0] for p in pts), min(p[1] for p in pts), max(p[0] for p in pts), max(p[1] for p in pts))


FRONT = Pose()
MID = Pose(theta=17, z=34, axis=15, foot_lift=-16, foot_z=-25, lift=-7, lean=-3, eye_pop=1.12)
LAND = Pose(theta=34, z=28, axis=30, lift=2, eye_pop=1.35)
SETTLE = LAND.with_(lift=0)
CLOSED = SETTLE.with_(wink=0.06)

INTRO_DUR = "1.8s"
# body + far eye: stand, hold, mid-step, land, settle, hold
POSE_KEYS = [FRONT, FRONT, MID, LAND, SETTLE, SETTLE]
POSE_TIMES = "0;.13;.24;.42;.5;1"
POSE_SPLINES = "0 0 1 1;.4 0 .6 1;.4 0 .6 1;.3 0 .5 1;0 0 1 1"
# near eye: same, then the wink
EYE_KEYS = POSE_KEYS[:5] + [SETTLE, CLOSED, SETTLE, SETTLE]
EYE_TIMES = "0;.13;.24;.42;.5;.52;.58;.66;1"
EYE_SPLINES = POSE_SPLINES[:-len(";0 0 1 1")] + ";0 0 1 1;.5 0 1 1;0 0 .5 1;0 0 1 1"


# rare variant: same entrance, then the near arm swings up and out in one motion, finger out
SWING_A = SETTLE.with_(arm_raise=14, arm_stretch=0.3, finger=0.9, arm_depth=14)
SWING_B = SETTLE.with_(arm_raise=48, arm_stretch=0.82, finger=1.06, arm_depth=12)
FINGER = SETTLE.with_(arm_raise=42, arm_stretch=0.75, finger=1.0, arm_depth=12)
ALT_KEYS = [FRONT, FRONT, MID, LAND, SETTLE, SETTLE, SWING_A, SWING_B, FINGER, FINGER]
ALT_TIMES = "0;.13;.24;.42;.5;.53;.57;.64;.7;1"
ALT_SPLINES = POSE_SPLINES[:-len(";0 0 1 1")] + ";0 0 1 1;.4 0 .8 1;.2 0 .6 1;.3 0 .6 1;0 0 1 1"
SHADE_FADE_TIMES = "0;.53;.57;1"
SHADE_FADE_VALUES = "0;0;1;1"


def animate(values, times, splines):
    return (f'<animate attributeName="d" dur="{INTRO_DUR}" fill="freeze" calcMode="spline" '
            f'keyTimes="{times}" keySplines="{splines}" values="{";".join(values)}"/>')


def intro_svg(rare=False):
    if rare:
        # the eyes share the gesture's keyframes so they follow the recentring as the hand extends
        solid_keys, solid_times, solid_splines = ALT_KEYS, ALT_TIMES, ALT_SPLINES
        far_keys, far_times, far_splines = ALT_KEYS, ALT_TIMES, ALT_SPLINES
        near_keys, near_times, near_splines = ALT_KEYS, ALT_TIMES, ALT_SPLINES
        mask_id, parts = "alt-cut", RARE_PARTS + SHADED_PARTS
    else:
        solid_keys, solid_times, solid_splines = POSE_KEYS, POSE_TIMES, POSE_SPLINES
        far_keys, far_times, far_splines = POSE_KEYS, POSE_TIMES, POSE_SPLINES
        near_keys, near_times, near_splines = EYE_KEYS, EYE_TIMES, EYE_SPLINES
        mask_id, parts = "in-cut", SOLID_PARTS
    solid_frames = frames_for(solid_keys, parts)
    far_frames = solid_frames if far_keys is solid_keys else frames_for(far_keys, parts)
    near_frames = solid_frames if near_keys is solid_keys else frames_for(near_keys, parts)
    solid = [d_of([f[part] for part in parts if part not in SHADED_PARTS]) for f in solid_frames]
    far = [d_of([f["eye_r"]]) for f in far_frames]
    near = [d_of([f["eye_l"]]) for f in near_frames]
    hand = ""
    if rare:
        # creases between the fingers sit on top of the masked red in the darker tone; they fade
        # in as the hand comes up
        hand_d = [d_of([f[part] for part in SHADED_PARTS]) for f in solid_frames]
        hand = (f'\n<path fill="{HAND}" fill-opacity="0" d="{hand_d[0]}">{animate(hand_d, solid_times, solid_splines)}'
                f'<animate attributeName="fill-opacity" dur="{INTRO_DUR}" fill="freeze" keyTimes="{SHADE_FADE_TIMES}" values="{SHADE_FADE_VALUES}"/></path>')
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-label="Vorcall">
<defs>
  <mask id="{mask_id}" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256">
    <path fill="#fff" d="{solid[0]}">{animate(solid, solid_times, solid_splines)}</path>
    <path fill="#000" d="{far[0]}">{animate(far, far_times, far_splines)}</path>
    <path fill="#000" d="{near[0]}">{animate(near, near_times, near_splines)}</path>
  </mask>
</defs>
<rect width="256" height="256" fill="{RED}" mask="url(#{mask_id})"/>{hand}
</svg>
'''


# ----------------------------------------------------------------------------
# Loading: two little loops on a 9.6 s cycle, creature facing the camera.
#   bob     x3  (1.6 s each)  0.0 - 4.8 s   up and down with a little compression at the bottom
#   typing  x1  (4.8 s)       4.8 - 9.6 s   laptop in front, hands on the keys, eyes scan the screen
# Each scene is its own masked copy of the creature so its eyes stay holes. Scene loops run on
# CSS with a delay equal to their window start; 9.6 is a multiple of both periods, so every
# window opens at the top of its loop.
# ----------------------------------------------------------------------------


LOAD_SCENE = 4.8              # one scene: the bob's window, and the whole typing loop
LOAD_PERIOD = LOAD_SCENE * 2  # the full cycle, both scenes
LOAD_BOB = 1.6                # one bob, up and back down; the scene is three of them
LOAD_TAP = 0.2                # one tap of a typing hand


def eye_el(cls, e):
    cx, cy, rx, ry = e
    return f'<ellipse class="{cls}" cx="{cx}" cy="{cy}" rx="{rx}" ry="{ry}" fill="#000"/>'


def typing_arm_keyframes(name, phase, period=LOAD_SCENE, tap=LOAD_TAP):
    """Taps during the two typing thirds of the loop; phase offsets the two hands."""
    k = []
    step = tap / period * 100
    for start, end in ((0, 33.3), (66.7, 100)):
        t = start
        up = phase
        while t < end:
            k.append(f"{t:.2f}%{{transform:translateY({-4 if up else 0}px)}}")
            up = not up
            t += step
    k.append("33.4%,66.6%{transform:translateY(0)}")
    return f"@keyframes {name}{{{''.join(k)}}}"


LOADING_CSS = f"""
.sc{{animation:{LOAD_PERIOD}s step-end infinite}}
.s1{{animation-name:ld-show1}}.s2{{animation-name:ld-show2}}
@keyframes ld-show1{{0%{{visibility:visible}}50%{{visibility:hidden}}100%{{visibility:hidden}}}}
@keyframes ld-show2{{0%{{visibility:hidden}}50%{{visibility:visible}}100%{{visibility:visible}}}}

.b-bob{{animation:ld-bob {LOAD_BOB}s ease-in-out infinite}}
.b-squash{{transform-origin:128px 226px;animation:ld-squash {LOAD_BOB}s ease-in-out infinite}}
@keyframes ld-bob{{0%,100%{{transform:translateY(0)}}50%{{transform:translateY(-6px)}}}}
@keyframes ld-squash{{0%,100%{{transform:scale(1.04,.96)}}50%{{transform:scale(.99,1.01)}}}}

.t-hand-l{{animation:ld-tap-l {LOAD_SCENE}s linear infinite {LOAD_SCENE}s}}
.t-hand-r{{animation:ld-tap-r {LOAD_SCENE}s linear infinite {LOAD_SCENE}s}}
.t-eyes{{animation:ld-scan {LOAD_SCENE}s linear infinite {LOAD_SCENE}s}}
@keyframes ld-scan{{0%,33%{{transform:translate(0,6px)}}35%{{transform:translate(-7px,5px)}}49%{{transform:translate(7px,5px)}}51%{{transform:translate(-7px,5px)}}65%{{transform:translate(7px,5px)}}67%,100%{{transform:translate(0,6px)}}}}
""" + typing_arm_keyframes("ld-tap-l", True) + "\n" + typing_arm_keyframes("ld-tap-r", False) + """
@media (prefers-reduced-motion:reduce){.sc,.b-bob,.b-squash,.t-hand-l,.t-hand-r,.t-eyes{animation:none}.s2{visibility:hidden}}
"""

# Laptop from the front, a touch above: a short deck strip with the keyboard, the hands on it,
# and the tall lid nearest the camera hiding the lower half of the hands.
DECK_PTS = [(66, 162), (190, 162), (196, 178), (60, 178)]
KEYS_RECT = (84, 165, 88, 11, 2)     # x, y, width, height, corner radius
LID_RECT = (58, 176, 140, 50, 7)
LOGO_XF = (128, 201, 0.1, 0.085)     # the mark on the lid: translate, scale about its centre
TYPING_ARM_W = 18
TYPING_HAND_R = 12
TYPING_L = ((66, 124), (108, 168))   # shoulder, hand on the keys
TYPING_R = ((190, 124), (148, 168))


def rect_el(rect, fill):
    x, y, w, h, rx = rect
    return f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}"/>'


DECK = (f'<path fill="{DECK_LIGHT}" d="M' + " L".join(f"{x} {y}" for x, y in DECK_PTS) + ' Z"/>'
        + rect_el(KEYS_RECT, KEYS))
LID = rect_el(LID_RECT, STEEL)


def typing_arm(cls, shoulder, hand):
    pts = capsule(shoulder, hand, TYPING_ARM_W)
    d = "M" + " L".join(f"{x:.1f} {y:.1f}" for x, y in pts) + " Z"
    return (f'<g class="{cls}"><path fill="#fff" d="{d}"/>'
            f'<circle cx="{hand[0]}" cy="{hand[1]}" r="{TYPING_HAND_R}" fill="#fff"/></g>')


def loading_svg():
    solids = f"{BODY} {HORN_L} {HORN_R} {FOOT_L} {FOOT_R}"

    def eyes(cls, dy=0):
        l, r = (EYE_L[0], EYE_L[1] + dy, EYE_L[2], EYE_L[3]), (EYE_R[0], EYE_R[1] + dy, EYE_R[2], EYE_R[3])
        return f'<g class="{cls}">{eye_el("", l)}{eye_el("", r)}</g>'

    arms = typing_arm("t-hand-l", *TYPING_L) + typing_arm("t-hand-r", *TYPING_R)
    tx, ty, sx, sy = LOGO_XF
    logo = (f'<path fill="{RED}" fill-rule="nonzero" transform="translate({tx} {ty}) scale({sx} {sy}) translate(-128 -128)" d="{MARK_D}"/>')
    return f'''<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" role="img" aria-label="Vorcall is loading">
<style>{LOADING_CSS}</style>
<defs>
  <mask id="ld1-cut" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256">
    <g class="b-bob"><g class="b-squash"><path fill="#fff" d="{solids} {ARM_L} {ARM_R}"/>{eyes("")}</g></g>
  </mask>
  <mask id="ld2-cut" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256">
    <path fill="#fff" transform="translate(0 -20)" d="{solids}"/>
    {arms}
    {eyes("t-eyes", -20)}
  </mask>
  <mask id="ld2-hands" maskUnits="userSpaceOnUse" x="0" y="0" width="256" height="256">
    {arms}
  </mask>
</defs>
<g class="sc s1"><rect width="256" height="256" fill="{RED}" mask="url(#ld1-cut)"/></g>
<g class="sc s2" visibility="hidden"><rect width="256" height="256" fill="{RED}" mask="url(#ld2-cut)"/>{DECK}<rect width="256" height="256" fill="{RED}" mask="url(#ld2-hands)"/>{LID}{logo}</g>
</svg>
'''


# ----------------------------------------------------------------------------
# Generated Rust module: the same geometry the SVGs carry, for the iced client.
# Solid rings keep a positive shoelace sign and the eyes a negative one, so a
# nonzero fill of the concatenated rings punches the eyes out as holes.
# ----------------------------------------------------------------------------

RUST_OUT = os.path.join(
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
    "client", "crates", "vorcall-app", "src", "brand", "data.rs")


def f2(v):
    return f"{v:.2f}"


def f3(v):
    """Three decimals, for the handful of values two would round away."""
    return f"{v:.3f}"


def shoelace(pts):
    return sum(pts[i][0] * pts[(i + 1) % len(pts)][1] - pts[(i + 1) % len(pts)][0] * pts[i][1]
               for i in range(len(pts)))


def reverse_chain(sub):
    """Same closed ring traversed the other way round; cubic control points swap."""
    nodes = [sub[0][1]]
    segs = []
    for cmd in sub[1:]:
        if cmd[0] == "L":
            segs.append(("L", None, None, cmd[1]))
        else:
            segs.append(("C", cmd[1], cmd[2], cmd[3]))
        nodes.append(segs[-1][3])
    out = [("M", nodes[-1])]
    for i in range(len(segs) - 1, -1, -1):
        kind, c1, c2, _ = segs[i]
        out.append(("L", nodes[i]) if kind == "L" else ("C", c2, c1, nodes[i]))
    return out


def oriented(sub, want_positive):
    s = shoelace(flatten(sub))
    if (s > 0) != want_positive:
        sub = reverse_chain(sub)
        s = shoelace(flatten(sub))
    if (s > 0) != want_positive:
        raise AssertionError(f"cannot orient chain: shoelace {s}")
    return sub


def mark_chain_groups():
    """The mark's rings in the three groups the loading scenes move independently."""
    body = [body_sub(),
            horn_sub(horn_points(*HORN_ARGS)),
            horn_sub(horn_points(*[mirror(p) for p in HORN_ARGS])),
            foot_sub(84), foot_sub(172)]
    arms = [arm_left_sub(), arm_right_sub()]
    eyes = [eye_sub(*EYE_L), eye_sub(*EYE_R)]
    return ([oriented(s, True) for s in body],
            [oriented(s, True) for s in arms],
            [oriented(e, False) for e in eyes])


def mark_chains():
    body, arms, eyes = mark_chain_groups()
    return body + arms + eyes


def rust_cmds(chains):
    """Closed outline chains as `Cmd` literals, one ring after another."""
    lines = []
    for sub in chains:
        for cmd in sub:
            if cmd[0] == "M":
                lines.append(f"    Cmd::M({f2(cmd[1][0])}, {f2(cmd[1][1])}),")
            elif cmd[0] == "L":
                lines.append(f"    Cmd::L({f2(cmd[1][0])}, {f2(cmd[1][1])}),")
            else:
                lines.append("    Cmd::C(" + ", ".join(f2(v) for p in cmd[1:] for v in p) + "),")
        lines.append("    Cmd::Z,")
    return "\n".join(lines)


def rust_mark():
    return rust_cmds(mark_chains())


def rust_chains(name, doc, chains):
    return "\n".join([doc, f"pub const {name}: &[Cmd] = &[", rust_cmds(chains), "];"])


def rust_polygon(name, doc, pts, per_line=6):
    lines = [doc, f"pub const {name}: &[[f32; 2]] = &["]
    for i in range(0, len(pts), per_line):
        lines.append("    " + " ".join(f"[{f2(x)}, {f2(y)}]," for x, y in pts[i:i + per_line]))
    return "\n".join(lines + ["];"])


def rust_array(name, doc, values, fmt=f2):
    return f"{doc}\npub const {name}: [f32; {len(values)}] = [" + ", ".join(fmt(v) for v in values) + "];"


def parse_numbers(spec):
    return [float(v) for v in spec.split(";")]


def parse_splines(spec):
    groups = [[float(v) for v in g.split()] for g in spec.split(";")]
    assert all(len(g) == 4 for g in groups), spec
    return groups


def frames_for(keys, parts):
    cache = {}
    out = []
    for p in keys:
        if id(p) not in cache:
            cache[id(p)] = project_rig(p, parts)
        out.append(cache[id(p)])
    return out


def solid_points(frame, order):
    pts = []
    for part in order:
        got = [cmd[1] for cmd in frame[part]]
        assert len(got) == POINTS[part], (part, len(got))
        assert shoelace(got) > 0, part
        pts += got
    return pts


def eye_points(frame, part):
    """The projected eye's four cubics at t = 0, 1/6 .. 5/6: 24 points, wound negative."""
    pts, cur = [], frame[part][0][1]
    for cmd in frame[part][1:]:
        assert cmd[0] == "C", cmd[0]
        p0, p1, p2, p3 = cur, cmd[1], cmd[2], cmd[3]
        for s in range(6):
            t, u = s / 6, 1 - s / 6
            pts.append((u ** 3 * p0[0] + 3 * u * u * t * p1[0] + 3 * u * t * t * p2[0] + t ** 3 * p3[0],
                        u ** 3 * p0[1] + 3 * u * u * t * p1[1] + 3 * u * t * t * p2[1] + t ** 3 * p3[1]))
        cur = p3
    assert len(pts) == 24, len(pts)
    if shoelace(pts) > 0:
        pts = [pts[0]] + pts[1:][::-1]
    assert shoelace(pts) < 0
    return pts


def rust_track(name, times_spec, splines_spec, keys, expect):
    times, splines = parse_numbers(times_spec), parse_splines(splines_spec)
    assert times[0] == 0.0 and times[-1] == 1.0, name
    assert all(b > a for a, b in zip(times, times[1:])), name
    assert len(splines) == len(times) - 1, name
    assert len(keys) == len(times), name
    assert all(len(k) == expect for k in keys), name
    body = [f"pub const {name}: Track = Track {{",
            "    times: &[" + ", ".join(f2(t) for t in times) + "],",
            "    splines: &[" + ", ".join("[" + ", ".join(f2(v) for v in g) + "]" for g in splines) + "],",
            "    keys: &["]
    for key in keys:
        body.append("        &[")
        for i in range(0, len(key), 10):
            body.append("            " + " ".join(f"[{f2(x)}, {f2(y)}]," for x, y in key[i:i + 10]))
        body.append("        ],")
    body += ["    ],", "};"]
    return "\n".join(body)


def rust_module():
    dur = float(INTRO_DUR.rstrip("s"))
    intro_counts = [POINTS[p] for p in SOLID_PARTS]
    rare_counts = [POINTS[p] for p in RARE_PARTS]
    crease_counts = [POINTS[p] for p in SHADED_PARTS]
    alpha_times = parse_numbers(SHADE_FADE_TIMES)
    alpha = parse_numbers(SHADE_FADE_VALUES)
    assert len(alpha) == len(alpha_times)

    load_body, load_arms, load_eyes = mark_chain_groups()

    wink = frames_for(POSE_KEYS, SOLID_PARTS)
    eye_wink = frames_for(EYE_KEYS, SOLID_PARTS)
    rare = frames_for(ALT_KEYS, RARE_PARTS + SHADED_PARTS)

    out = ["//! Generated by `assets/brand/gen.py`; do not edit by hand.",
           "//!",
           "//! The mark and the entrance keyframes, in the 256-unit box of the SVGs.",
           "// Coordinates rounded to two decimals hit constants such as 3.14 by chance.",
           "#![allow(clippy::approx_constant)]",
           "",
           "/// Side of the box every coordinate lives in (the SVG viewBox).",
           "pub const VIEW: f32 = 256.0;",
           "/// Length of the entrance, both variants, in seconds.",
           f"pub const DURATION_SECS: f32 = {f2(dur)};",
           "",
           "#[derive(Clone, Copy, Debug)]",
           "pub enum Cmd {",
           "    M(f32, f32),",
           "    L(f32, f32),",
           "    C(f32, f32, f32, f32, f32, f32),",
           "    Z,",
           "}",
           "",
           "/// The static mark: solids wound one way, eyes the other, for nonzero fill.",
           "pub const MARK: &[Cmd] = &[",
           rust_mark(),
           "];",
           "",
           "/// A morph track. `keys[k]` holds the points at `times[k]`; every key has the same length.",
           "/// `splines.len() == times.len() - 1`; entry k is the SMIL keySpline `[x1, y1, x2, y2]`",
           "/// easing the segment `times[k]..times[k + 1]`.",
           "pub struct Track {",
           "    pub times: &'static [f32],",
           "    pub splines: &'static [[f32; 4]],",
           "    pub keys: &'static [&'static [[f32; 2]]],",
           "}",
           "",
           "/// Point count of each closed subpath inside one solid key, in order.",
           "pub const INTRO_SOLID_COUNTS: &[usize] = &[" + ", ".join(str(c) for c in intro_counts) + "];",
           rust_track("INTRO_SOLID", POSE_TIMES, POSE_SPLINES,
                      [solid_points(f, SOLID_PARTS) for f in wink], sum(intro_counts)),
           rust_track("INTRO_EYE_FAR", POSE_TIMES, POSE_SPLINES,
                      [eye_points(f, "eye_r") for f in wink], 24),
           rust_track("INTRO_EYE_NEAR", EYE_TIMES, EYE_SPLINES,
                      [eye_points(f, "eye_l") for f in eye_wink], 24),
           "",
           "pub const RARE_SOLID_COUNTS: &[usize] = &[" + ", ".join(str(c) for c in rare_counts) + "];",
           rust_track("RARE_SOLID", ALT_TIMES, ALT_SPLINES,
                      [solid_points(f, RARE_PARTS) for f in rare], sum(rare_counts)),
           rust_track("RARE_EYE_FAR", ALT_TIMES, ALT_SPLINES,
                      [eye_points(f, "eye_r") for f in rare], 24),
           rust_track("RARE_EYE_NEAR", ALT_TIMES, ALT_SPLINES,
                      [eye_points(f, "eye_l") for f in rare], 24),
           "",
           "pub const RARE_CREASE_COUNTS: &[usize] = &[" + ", ".join(str(c) for c in crease_counts) + "];",
           rust_track("RARE_CREASES", ALT_TIMES, ALT_SPLINES,
                      [solid_points(f, SHADED_PARTS) for f in rare], sum(crease_counts)),
           "/// Crease opacity, linear between these (SMIL `values=\"0;0;1;1\"` on SHADE_FADE_TIMES).",
           "pub const RARE_CREASE_ALPHA_TIMES: &[f32] = &[" + ", ".join(f2(t) for t in alpha_times) + "];",
           "pub const RARE_CREASE_ALPHA: &[f32] = &[" + ", ".join(f2(a) for a in alpha) + "];",
           "",
           rust_chains("LOADING_BODY",
                       "/// The loading creature (`assets/brand/loading.svg`): body, horns and feet, wound\n"
                       "/// like the solids of `MARK`. Its arms and eyes are separate rings because the\n"
                       "/// scenes move them on their own.",
                       load_body),
           rust_chains("LOADING_ARMS",
                       "/// The resting arms; the bob scene draws them, the typing scene swaps them for\n"
                       "/// `LOADING_TYPING_ARM_L` and `LOADING_TYPING_ARM_R`.",
                       load_arms),
           rust_chains("LOADING_EYES",
                       "/// Both eyes at rest, wound against the body so a nonzero fill cuts them out.",
                       load_eyes),
           rust_polygon("LOADING_TYPING_ARM_L",
                        "/// The left arm reaching from the shoulder down to the keys, as a rounded bar.",
                        capsule(*TYPING_L, TYPING_ARM_W)),
           rust_polygon("LOADING_TYPING_ARM_R",
                        "/// The right arm reaching from the shoulder down to the keys.",
                        capsule(*TYPING_R, TYPING_ARM_W)),
           rust_array("LOADING_HAND_L", "/// The left hand on the keys: centre and radius.",
                      (*TYPING_L[1], TYPING_HAND_R)),
           rust_array("LOADING_HAND_R", "/// The right hand on the keys: centre and radius.",
                      (*TYPING_R[1], TYPING_HAND_R)),
           rust_polygon("LOADING_DECK", "/// The laptop deck the hands rest on.", DECK_PTS),
           rust_array("LOADING_KEYS", "/// The keyboard: x, y, width, height, corner radius.", KEYS_RECT),
           rust_array("LOADING_LID", "/// The lid, nearest the camera: x, y, width, height, corner radius.",
                      LID_RECT),
           rust_array("LOADING_LOGO",
                      "/// The mark on the lid: `translate(tx, ty) scale(sx, sy) translate(-128, -128)`.",
                      LOGO_XF, f3),
           "/// One loading cycle: the bob scene, then the typing scene.",
           f"pub const LOADING_PERIOD_SECS: f32 = {f2(LOAD_PERIOD)};",
           "/// One scene of the cycle.",
           f"pub const LOADING_SCENE_SECS: f32 = {f2(LOAD_SCENE)};",
           "/// One bob, up and back down; the bob scene is three of them.",
           f"pub const LOADING_BOB_SECS: f32 = {f2(LOAD_BOB)};",
           ""]
    return "\n".join(out)

# ----------------------------------------------------------------------------
# --- icons ---
# The UI icon set: one grid, one stroke weight, no colour of its own. The client
# embeds these and tints them through iced's `svg::Style { color }`, so every
# shape is stroked with `currentColor` and nothing is filled but a few dots.
# Run:  python3 assets/brand/gen.py icons assets/icons      (no shapely needed)
# ----------------------------------------------------------------------------

ICON_HEAD = ('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" '
             'stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round">')
ICON_SLASH = "M4 4 L20 20"      # the bar across every "off" variant


def inum(v):
    """At most one decimal, no trailing zero, no negative zero."""
    s = f"{v:.1f}"
    if s.endswith(".0"):
        s = s[:-2]
    return "0" if s == "-0" else s


def iring(cx, cy, r):
    """A stroked circle, as two half arcs."""
    return (f"M{inum(cx - r)} {inum(cy)} a{inum(r)} {inum(r)} 0 1 0 {inum(2 * r)} 0 "
            f"a{inum(r)} {inum(r)} 0 1 0 {inum(-2 * r)} 0")


def idisc(cx, cy, r):
    """A filled dot: the one element that carries a fill, marked for the writer."""
    return ("fill", iring(cx, cy, r))


def irect(x0, y0, x1, y1, r):
    return (f"M{inum(x0 + r)} {inum(y0)} H{inum(x1 - r)} A{inum(r)} {inum(r)} 0 0 1 {inum(x1)} {inum(y0 + r)} "
            f"V{inum(y1 - r)} A{inum(r)} {inum(r)} 0 0 1 {inum(x1 - r)} {inum(y1)} H{inum(x0 + r)} "
            f"A{inum(r)} {inum(r)} 0 0 1 {inum(x0)} {inum(y1 - r)} V{inum(y0 + r)} "
            f"A{inum(r)} {inum(r)} 0 0 1 {inum(x0 + r)} {inum(y0)} Z")


def ipoly(pts, close=True):
    return "M" + " L".join(f"{inum(x)} {inum(y)}" for x, y in pts) + (" Z" if close else "")


def igear(cx=12, cy=12, r_in=2.8, r_body=7.2, r_tooth=9.2, teeth=8):
    """Hub, body ring and the teeth as radial stubs: a cog that still reads at 16 px."""
    out = [iring(cx, cy, r_in), iring(cx, cy, r_body)]
    for k in range(teeth):
        a = 2 * math.pi * k / teeth
        out.append(f"M{inum(cx + r_body * math.cos(a))} {inum(cy + r_body * math.sin(a))} "
                   f"L{inum(cx + r_tooth * math.cos(a))} {inum(cy + r_tooth * math.sin(a))}")
    return out


def istar(cx=12, cy=12.4, r_out=9, r_in=3.9, points=5):
    pts = []
    for k in range(2 * points):
        a = -math.pi / 2 + math.pi * k / points
        r = r_out if k % 2 == 0 else r_in
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return ipoly(pts)


SPEAKER_CONE = "M4 10 H8 L13 6 V18 L8 14 H4 Z"
MIC_BODY = ["M9 6 a3 3 0 0 1 6 0 V11 a3 3 0 0 1 -6 0 Z", "M5 11 a7 7 0 0 0 14 0", "M12 18 V21"]
HEADPHONES = ["M4 14 a8 8 0 0 1 16 0", "M4 14 V18.5 a1.5 1.5 0 0 0 3 0 V14",
              "M17 14 V18.5 a1.5 1.5 0 0 0 3 0 V14"]
SCREEN = [irect(3, 5, 21, 17, 2), "M12 17 V21", "M8 21 H16"]
BELL = ["M6 17 V11 a6 6 0 0 1 12 0 V17", "M4.5 17 H19.5", "M10.5 19.5 a1.5 1.5 0 0 0 3 0"]

ICONS = {
    "hash": ["M5 9.5 H19", "M5 14.5 H19", "M10.5 4 L9 20", "M16 4 L14.5 20"],
    "speaker": [SPEAKER_CONE, "M16.5 9 a4.5 4.5 0 0 1 0 6"],
    "speaker_off": [SPEAKER_CONE, "M16 9.5 L21 14.5", "M21 9.5 L16 14.5"],
    "mic": MIC_BODY,
    "mic_off": MIC_BODY + [ICON_SLASH],
    "headphones": HEADPHONES,
    "headphones_off": HEADPHONES + [ICON_SLASH],
    "screen": SCREEN,
    "screen_off": SCREEN + [ICON_SLASH],
    "chevron_down": ["M6 9 L12 15 L18 9"],
    "chevron_right": ["M9 6 L15 12 L9 18"],
    "plus": ["M12 5 V19", "M5 12 H19"],
    "gear": igear(),
    "bell": BELL,
    "bell_off": BELL + [ICON_SLASH],
    "users": [iring(9.5, 8, 3.4), "M3 20.5 a6.5 6.5 0 0 1 13 0",
              iring(17.6, 9, 2.6), "M16.6 14.8 a5.2 5.2 0 0 1 4.4 5.7"],
    "user": [iring(12, 8, 4), "M4 20.5 a8 6 0 0 1 16 0"],
    "search": [iring(11, 11, 6), "M15.5 15.5 L20 20"],
    "reply": ["M9 14 L4 9 L9 4", "M4 9 H13 a7 7 0 0 1 7 7 V19"],
    "edit": ["M4 20 V16 L16 4 L20 8 L8 20 Z", "M13 7 L17 11"],
    "trash": ["M4 7 H20", "M9 7 V4 H15 V7", "M6 7 l1 13 h10 l1 -13", "M10 11 V17", "M14 11 V17"],
    "smile": [iring(12, 12, 9), "M8 14 a5 4 0 0 0 8 0", "M9 9.5 V10.5", "M15 9.5 V10.5"],
    "paperclip": ["M21 12 l-8.5 8.5 a5 5 0 0 1 -7 -7 L14 5 a3.5 3.5 0 0 1 5 5 "
                  "l-8.5 8.5 a2 2 0 0 1 -3 -3 L15 8"],
    "close": ["M6 6 L18 18", "M18 6 L6 18"],
    "check": ["M5 12 L9 16 L19 6"],
    "dots": [idisc(6, 12, 1.25), idisc(12, 12, 1.25), idisc(18, 12, 1.25)],
    "shield": ["M12 3 L19 6 V12 c0 4.5 -3 7.5 -7 9 c-4 -1.5 -7 -4.5 -7 -9 V6 Z"],
    "crown": ["M4 17 L5 8 L9 12 L12 6 L15 12 L19 8 L20 17 Z"],
    "pin": ["M10 10 H4 V20 H14 V14", "M12 12 L20 4", "M14 4 H20 V10"],
    "expand": ["M4 9 V4 H9", "M20 15 V20 H15", "M5.5 5.5 L18.5 18.5"],
    "link": ["M10 16.5 H8.5 a4.5 4.5 0 0 1 0 -9 H10", "M14 7.5 H15.5 a4.5 4.5 0 0 1 0 9 H14",
             "M8.5 12 H15.5"],
    "ban": [iring(12, 12, 9), "M5.6 5.6 L18.4 18.4"],
    "boot": ["M4 3 H14 V21 H4 Z", "M14 12 H21", "M18 9 L21 12 L18 15"],
    "move": ["M12 5 V19", "M5 12 H19", "M9.5 7.5 L12 5 L14.5 7.5", "M9.5 16.5 L12 19 L14.5 16.5",
             "M7.5 9.5 L5 12 L7.5 14.5", "M16.5 9.5 L19 12 L16.5 14.5"],
    "star": [istar()],
    "drag": [idisc(9, 6, 1.1), idisc(15, 6, 1.1), idisc(9, 12, 1.1),
             idisc(15, 12, 1.1), idisc(9, 18, 1.1), idisc(15, 18, 1.1)],
    "arrow_up": ["M12 19 V5", "M6 11 L12 5 L18 11"],
    "arrow_down": ["M12 5 V19", "M6 13 L12 19 L18 13"],
    "image": [irect(3, 4, 21, 20, 2.5), iring(9, 10, 1.5), "M21 16 L16 11 L8 20"],
    "palette": [iring(12, 12, 8.5), idisc(9, 9.5, 1.2), idisc(13, 8, 1.2),
                idisc(16.2, 11.5, 1.2), idisc(14.5, 15.8, 1.2)],
    "keyboard": [irect(3, 6, 21, 18, 2.5), "M6 10.5 H7", "M9.5 10.5 H10.5", "M13 10.5 H14",
                 "M16.5 10.5 H17.5", "M8 14.5 H16"],
    "logout": ["M12 3 H4 V21 H12", "M10 12 H20", "M17 9 L20 12 L17 15"],
    "info": [iring(12, 12, 9), idisc(12, 8, 1.05), "M12 11 V16"],
    "warning": ["M12 3.5 L21 19.5 H3 Z", "M12 9 V13.5", idisc(12, 16.8, 1.05)],
}


def icon_file(parts):
    """One icon: stroked path commands, plus any filled dot marked by `idisc`."""
    body = "".join(f'<path fill="currentColor" stroke="none" d="{p[1]}"/>' if isinstance(p, tuple)
                   else f'<path d="{p}"/>' for p in parts)
    return f"{ICON_HEAD}{body}</svg>\n"


def write_icons(outdir):
    assert len(ICONS) == 44, len(ICONS)
    os.makedirs(outdir, exist_ok=True)
    written = []
    for name, parts in ICONS.items():
        assert name == name.lower() and name.replace("_", "").isalnum(), name
        text = icon_file(parts)
        assert len(text) <= 1024, (name, len(text))
        with open(os.path.join(outdir, f"{name}.svg"), "w") as fh:
            fh.write(text)
        written.append((name, len(text)))
    return written


if __name__ == "__main__":
    if sys.argv[1:2] == ["icons"]:
        if len(sys.argv) < 3:
            sys.exit("usage: gen.py icons <outdir>")
        for name, size in write_icons(sys.argv[2]):
            print(f"{name}.svg {size}")
        print("wrote", len(ICONS), "icons to", sys.argv[2])
        sys.exit(0)
    outdir = sys.argv[1] if len(sys.argv) > 1 else "."
    os.makedirs(outdir, exist_ok=True)
    for name, fn in (("mark", mark_svg), ("icon", icon_svg), ("intro", intro_svg),
                     ("intro-rare", lambda: intro_svg(rare=True)), ("loading", loading_svg)):
        with open(os.path.join(outdir, f"{name}.svg"), "w") as fh:
            fh.write(fn())
    for label, pose in (("front", FRONT), ("mid", MID), ("land", LAND), ("finger", FINGER)):
        f = project_rig(pose, RARE_PARTS + SHADED_PARTS)
        print(label, "bbox", tuple(round(v) for v in bbox([f[p] for p in RARE_PARTS])))
    rust_path = sys.argv[2] if len(sys.argv) > 2 else RUST_OUT
    os.makedirs(os.path.dirname(rust_path) or ".", exist_ok=True)
    with open(rust_path, "w") as fh:
        fh.write(rust_module())
    print("wrote", outdir)
    print("wrote", rust_path)
