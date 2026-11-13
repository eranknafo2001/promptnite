"""Models the teleporter pad and exports it to client/assets/teleporter.gltf.

Run it with Blender, or with Blender's Python module (`pip install bpy`):

    blender --background --python assets/blender/teleporter.py
    python assets/blender/teleporter.py

The pad is three meshes, which the client loads one by one and gives its own
materials (`client/src/teleporter.rs`), so their order matters:

    Mesh0  Pad    the dark metal dish
    Mesh1  Glow   the lit hub and segments set into it
    Mesh2  Beam   the flame that rises while somebody teleports. Its vertex
                  colours hold how see-through it is: solid at the foot,
                  gone at the tip.

One unit is one arena unit. Blender's Z is up; the exporter turns the model
so that the game's Y is.
"""

import math
import os

import bpy

import bmesh  # Part of Blender: only there once bpy is loaded.

# Outer radius of the pad. `TELEPORTER_RADIUS` in sim/src/rules.rs.
RADIUS = 0.9
# Corners of a full turn. Enough to look round from the game's camera.
TURN = 48

SEGMENTS = 8
# Width of the dark line between two lit segments, at any distance from the
# centre.
GAP = 0.05

BEAM_HEIGHT = 2.6

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "client", "assets", "teleporter.gltf")


def material(name, colour, metallic=0.0, roughness=0.5, emission=None, strength=0.0):
    mat = bpy.data.materials.new(name)
    mat.use_nodes = True
    shader = mat.node_tree.nodes["Principled BSDF"]
    shader.inputs["Base Color"].default_value = (*colour, 1.0)
    shader.inputs["Metallic"].default_value = metallic
    shader.inputs["Roughness"].default_value = roughness
    if emission:
        shader.inputs["Emission Color"].default_value = (*emission, 1.0)
        shader.inputs["Emission Strength"].default_value = strength
    return mat


def finish(name, bm, mat, smooth):
    """Turn a bmesh into an object in the scene."""
    for face in bm.faces:
        face.smooth = smooth
    bmesh.ops.recalc_face_normals(bm, faces=bm.faces)
    mesh = bpy.data.meshes.new(name)
    bm.to_mesh(mesh)
    bm.free()
    mesh.materials.append(mat)
    obj = bpy.data.objects.new(name, mesh)
    bpy.context.scene.collection.objects.link(obj)
    return obj


def ring(bm, radius, z, wobble=None):
    """One circle of vertices, or a single one on the axis."""
    if radius < 1e-6:
        return [bm.verts.new((0.0, 0.0, z))]
    verts = []
    for i in range(TURN):
        angle = math.tau * i / TURN
        r = radius * (wobble(angle) if wobble else 1.0)
        verts.append(bm.verts.new((r * math.cos(angle), r * math.sin(angle), z)))
    return verts


def band(bm, lower, upper):
    """Faces between two rings from `ring`."""
    for i in range(TURN):
        j = (i + 1) % TURN
        if len(lower) == 1:
            bm.faces.new((lower[0], upper[j], upper[i]))
        elif len(upper) == 1:
            bm.faces.new((lower[i], lower[j], upper[0]))
        else:
            bm.faces.new((lower[i], lower[j], upper[j], upper[i]))


def pad():
    """The dish: a sloped wall, a raised rim and a sunken floor."""
    profile = [
        (RADIUS, 0.0),
        (RADIUS - 0.03, 0.09),
        (RADIUS - 0.06, 0.12),
        (RADIUS - 0.15, 0.12),
        (RADIUS - 0.18, 0.05),
        (0.0, 0.05),
    ]
    bm = bmesh.new()
    # Every band gets rings of its own, so it is smooth around the pad and
    # has a crisp edge where the profile turns.
    for (r0, z0), (r1, z1) in zip(profile, profile[1:]):
        band(bm, ring(bm, r0, z0), ring(bm, r1, z1))
    metal = material("Metal", (0.035, 0.045, 0.06), metallic=0.85, roughness=0.42)
    return finish("Pad", bm, metal, smooth=True)


def slab(bm, outline, bottom, top):
    """A flat-topped block with the given outline, seen from above."""
    lower = [bm.verts.new((x, y, bottom)) for x, y in outline]
    upper = [bm.verts.new((x, y, top)) for x, y in outline]
    bm.faces.new(upper)
    for i in range(len(outline)):
        j = (i + 1) % len(outline)
        bm.faces.new((lower[i], lower[j], upper[j], upper[i]))


def arc(radius, start, end, steps):
    return [
        (radius * math.cos(a), radius * math.sin(a))
        for a in (start + (end - start) * i / steps for i in range(steps + 1))
    ]


def glow():
    """The lit parts: a round hub with a ring of segments around it."""
    bottom, top = 0.05, 0.072
    bm = bmesh.new()
    slab(bm, arc(0.2, 0.0, math.tau, TURN)[:-1], bottom, top)

    inner, outer = 0.2 + GAP, RADIUS - 0.18 - GAP
    for i in range(SEGMENTS):
        centre = math.tau * (i + 0.5) / SEGMENTS
        half = math.pi / SEGMENTS
        # Pull each end in by half a gap, measured along the arc, so that the
        # dark lines are as wide near the hub as at the rim.
        near = half - math.asin(GAP / 2 / inner)
        far = half - math.asin(GAP / 2 / outer)
        outline = arc(outer, centre - far, centre + far, 6)
        outline += arc(inner, centre + near, centre - near, 3)
        slab(bm, outline, bottom, top)

    cyan = material("Glow", (0.02, 0.35, 0.45), roughness=0.3, emission=(0.1, 0.85, 1.0), strength=4.0)
    return finish("Glow", bm, cyan, smooth=False)


def beam():
    """A flame: two open shells that narrow, sway and fade towards the tip."""
    bm = bmesh.new()
    alpha = bm.loops.layers.color.new("Color")
    rows = 14

    for scale, strength, turns in ((1.0, 0.55, 3), (0.55, 1.0, 2)):
        rings, fade = [], []
        for row in range(rows + 1):
            t = row / rows
            # Wide at the foot, pinched towards the tip.
            radius = scale * (RADIUS - 0.24) * (1.0 - 0.6 * t**1.4)
            # Tongues of flame that twist as they rise.
            wobble = lambda angle, t=t: 1.0 + 0.1 * t * math.sin(turns * angle + 5.0 * t)
            rings.append(ring(bm, radius, 0.08 + t * BEAM_HEIGHT * (0.75 + 0.25 * scale), wobble))
            fade.append(strength * (1.0 - t) ** 1.6)
        for row in range(rows):
            band(bm, rings[row], rings[row + 1])
        height = {v: f for verts, f in zip(rings, fade) for v in verts}
        for face in bm.faces:
            for loop in face.loops:
                if loop.vert in height:
                    loop[alpha] = (1.0, 1.0, 1.0, height[loop.vert])

    light = material("Beam", (0.1, 0.85, 1.0), emission=(0.1, 0.85, 1.0), strength=2.0)
    obj = finish("Beam", bm, light, smooth=True)
    obj.data.color_attributes.active_color = obj.data.color_attributes["Color"]
    obj.data.color_attributes.render_color_index = 0
    return obj


def main():
    bpy.ops.wm.read_factory_settings(use_empty=True)
    for build in (pad, glow, beam):
        build()
    bpy.ops.export_scene.gltf(
        filepath=os.path.normpath(OUT),
        export_format="GLTF_SEPARATE",
        export_yup=True,
        export_apply=True,
        export_vertex_color="ACTIVE",
        export_all_vertex_colors=False,
        export_active_vertex_color_when_no_material=True,
        export_animations=False,
        export_cameras=False,
        export_lights=False,
    )


if __name__ == "__main__":
    main()
