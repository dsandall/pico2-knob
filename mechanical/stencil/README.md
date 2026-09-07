# 3D-printed solder paste stencils

Generated with KiKit from `board_pico2knob/pico2-knob.kicad_pcb`.

## Which file to print

| Process | Top side | Bottom side |
|---|---|---|
| **FDM, 0.4 mm nozzle** | `topStencil-fdm.stl` | `bottomStencil-fdm-noJ3.stl` |
| **Resin (MSLA)** | `topStencil.stl` | `bottomStencil-noJ3.stl` |

The `-fdm` files have apertures enlarged 0.05 mm per side to offset FDM hole
shrinkage. Print them on FDM only — on resin they would over-dispense.

J3 is omitted from every bottom stencil meant for printing. `bottomStencil.stl`
retains it for reference only; see [J3](#j3).

## Regenerate

```sh
# resin
kikit stencil createprinted --pcbthickness 1.6 --thickness 0.2 \
  --framewidth 1.5 --frameclearance 0.1 \
  board_pico2knob/pico2-knob.kicad_pcb mechanical/stencil

# FDM (both sides, J3 omitted)
kikit stencil createprinted --pcbthickness 1.6 --thickness 0.2 \
  --framewidth 1.5 --frameclearance 0.1 --enlargeholes 0.05 --ignore J3 \
  board_pico2knob/pico2-knob.kicad_pcb mechanical/stencil/_fdm
```

KiKit always writes `topStencil.*` and `bottomStencil.*` into the output dir, so
the second run needs its files renamed to the `-fdm` names above and their
`.scad` DXF paths repointed at the top-level `.dxf` files. `--enlargeholes` is
applied in OpenSCAD, not in the DXF export, so both runs emit identical DXFs and
only one copy is kept.

Add `--ignore J3` to the first command for `bottomStencil-noJ3.*`.

## Parameters

| Option | Value | Why |
|---|---|---|
| `--pcbthickness` | 1.6 mm | board stackup thickness |
| `--thickness` | 0.2 mm | aperture depth = paste volume. 0.15 is closer to a steel stencil but is fragile printed; 0.3 would over-dispense onto U2's 0.6 mm pads and bridge |
| `--framewidth` | 1.5 mm | registration lip around the outline |
| `--frameclearance` | 0.1 mm | slip fit over the PCB edge; 0 is a press fit that print tolerance won't hit |
| `--enlargeholes` | 0.05 mm | FDM only — recovers hole shrinkage |

## Geometry

All variants 54.15 × 102.72 × 1.5 mm, watertight (0 non-manifold edges).
Height is 0.2 mm of stencil plate plus a 1.3 mm register lip that grips the
board edge.

Measured from the exported meshes, sliced mid-plate at z = 0.1 mm:

| File | Apertures | Min width | Min web | Web @ 0.33 mm extrusion |
|---|---|---|---|---|
| `topStencil.stl` | 19 | 0.600 mm | 0.655 mm | 2.0× |
| `topStencil-fdm.stl` | 19 | 0.700 mm | 0.553 mm | 1.7× |
| `bottomStencil-noJ3.stl` | 8 | 1.149 mm | 1.266 mm | 3.8× |
| `bottomStencil-fdm-noJ3.stl` | 8 | 1.249 mm | 1.166 mm | 3.5× |

The *web* is the wall left standing between adjacent apertures. On FDM it must
hold at least one extrusion, which is what constrains this board — not the
aperture width.

Per-part, from the board:

| Part | Side | Min aperture | Min web | Area ratio @0.2 mm |
|---|---|---|---|---|
| L1 | B | 2.90 mm | 7.00 mm | 4.72 |
| D1 | B | 1.80 mm | 1.50 mm | 2.62 |
| C1–C6 | F/B | 1.15 mm | 1.80 mm | 1.75 |
| U2 SOIC-8 | F | 0.60 mm | 0.67 mm | 1.15 |
| Q1 SOT-23 | F | 0.60 mm | 0.66 mm | 1.07 |
| J3 FPC | B | 0.30 mm | **0.20 mm** | **0.61** |

Area ratio > 0.66 is the paste-release rule, independent of process.

## Slicer settings — FDM

- **Extrusion width 0.33 mm.** This is the one setting that matters. The tightest
  web is 0.553 mm (Q1, U2); at the default 0.42 mm width that is 1.3 extrusions
  and the slicer may merge the two perimeters into a blob or drop the wall
  entirely. At 0.33 mm it is 1.7×, enough for a clean wall.
- **Layer height 0.1 mm**, so the 0.2 mm plate is 2 layers rather than 1. A
  single-layer sheet this size is floppy and delaminates from the frame.
- No supports inside the apertures; print plate-side down, flat on the bed.
- Turn off elephant-foot compensation — it eats the aperture edges, which the
  0.05 mm enlargement has already accounted for.

`bottomStencil-fdm-noJ3.stl` is undemanding (1.166 mm webs) and prints on
defaults. Only the top stencil needs the width change.

## J3

`bottomStencil.stl` includes J3 (ER-CON20HT-1, 0.3 mm apertures on 0.5 mm
pitch). Its 0.20 mm web is under half an extrusion, and its 0.61 area ratio
fails the release rule even for laser-cut steel at this thickness. It is kept
only as a reference for ordering a thinner steel stencil later. Drag-solder or
hot-air J3 by hand.

## Use

1. Paste and reflow the bottom side first, while the top is still bare and sits
   flat.
2. Then the top side. L1 (12×12×6 mm) and U1 on the bottom stand proud, so
   support the board on a jig or foam — the stencil registers only on the
   perimeter and will rock otherwise.
3. Hand-solder J3.
4. THT parts (R1–R8, SW1–SW3, RE1, J2) go in after both reflows.

## Files

`*.scad` is the OpenSCAD source; `*.dxf` are the paste and Edge.Cuts exports it
imports. The `.scad` files reference the `.dxf` files by **absolute path**, so
moving this directory breaks re-rendering — but not the already-exported STLs.
Regenerate rather than relocate.
