# SPDX-License-Identifier: Apache-2.0
"""Draws the Koidrive wordmark in Outfit 600 and trims the supplied nav icon's viewBox.

    python3 -m venv .venv && .venv/bin/pip install fonttools brotli
    .venv/bin/python site/logo/draw.py /path/to/outfit-latin-600-normal.woff2

Setting: the font's own pair kerning and -0.01 em of tracking after every letter. Writes wordmark.svg next to this
file: the wordmark in the ink, for the dark paper, cropped to its ink. Only its outlines ship, not the source font.
The nav icon keeps the supplied paths. icons.mjs renders the owner's app icon and the OG image.
"""
import argparse
import math
import re
from pathlib import Path
from xml.etree import ElementTree

from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.ttLib import TTFont
from fontTools.svgLib.path import parse_path

HERE = Path(__file__).resolve().parent
TRACK = -0.01  # em, after every letter
INK = '#efebe4'  # --color-ink, oklch(94% 0.010 80)
WORDMARK_CAP = 100  # cap height of the wordmark in its own units


def pair_kern(font, left, right):
    """The GPOS pair adjustment (x advance of the first glyph) for two glyph names, in font units."""
    total = 0
    for lookup in font['GPOS'].table.LookupList.Lookup:
        for st in lookup.SubTable:
            if lookup.LookupType == 9:
                st = st.ExtSubTable
            if st.LookupType != 2 or left not in st.Coverage.glyphs:
                continue
            if st.Format == 1:
                for rec in st.PairSet[st.Coverage.glyphs.index(left)].PairValueRecord:
                    if rec.SecondGlyph == right and rec.Value1:
                        total += getattr(rec.Value1, 'XAdvance', 0) or 0
            else:
                rec = st.Class1Record[st.ClassDef1.classDefs.get(left, 0)].Class2Record[st.ClassDef2.classDefs.get(right, 0)]
                if rec.Value1:
                    total += getattr(rec.Value1, 'XAdvance', 0) or 0
    return total


def outline(font, text, scale, dx, baseline):
    """The text as one SVG path (font units scaled, y flipped, the pen at dx, the baseline at y) and its ink bounds."""
    upm, cmap, hmtx = font['head'].unitsPerEm, font.getBestCmap(), font['hmtx']
    glyphs = font.getGlyphSet()
    names = [cmap[ord(c)] for c in text]
    pen, bounds = SVGPathPen(glyphs, ntos=lambda v: f'{round(v, 3):g}'), BoundsPen(glyphs)
    x = 0.0
    for i, name in enumerate(names):
        t = (scale, 0, 0, -scale, dx + x * scale, baseline)
        glyphs[name].draw(TransformPen(pen, t))
        glyphs[name].draw(TransformPen(bounds, t))
        if i < len(names) - 1:
            x += hmtx[name][0] + pair_kern(font, name, names[i + 1]) + TRACK * upm
    return pen.getCommands(), bounds.bounds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('font', type=Path, help='Outfit 600 Latin WOFF2 from @fontsource/outfit 5.3.0')
    args = parser.parse_args()
    font = TTFont(args.font)
    if font['OS/2'].usWeightClass != 600:
        parser.error('The wordmark requires Outfit weight 600')
    cap = font['OS/2'].sCapHeight

    d, (x0, y0, x1, y1) = outline(font, 'Koidrive', WORDMARK_CAP / cap, 0, 0)
    (HERE / 'wordmark.svg').write_text(
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{x0:.2f} {y0:.2f} {x1 - x0:.2f} {y1 - y0:.2f}">'
        f'<path d="{d}" fill="{INK}"/></svg>\n')

    print(f'wordmark.svg: viewBox {x0:.2f} {y0:.2f} {x1 - x0:.2f} {y1 - y0:.2f}')

    mark = (HERE / 'koidrive-bold-icon.svg').read_text()
    bounds = BoundsPen(None)
    for path in ElementTree.fromstring(mark).iter('{http://www.w3.org/2000/svg}path'):
        parse_path(path.attrib['d'], bounds)
    x0, y0, x1, y1 = bounds.bounds
    x0, y0, x1, y1 = math.floor(x0), math.floor(y0), math.ceil(x1), math.ceil(y1)
    box = f'{x0} {y0} {x1 - x0} {y1 - y0}'
    (HERE / 'nav-icon.svg').write_text(re.sub(r'viewBox="[^"]+"', f'viewBox="{box}"', mark, count=1))
    print(f'nav-icon.svg: viewBox {box}; paths unchanged')


if __name__ == '__main__':
    main()
