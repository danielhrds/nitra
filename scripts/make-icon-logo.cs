using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;

// Builds the logo for scripts/make-icon.ps1 from its letters drawn 8x larger,
// one picture per letter at its place in the word (white on transparent):
// the letters with a straight stem on the left move so it falls on a whole
// output pixel (sharp, where the browser left it between two), the "n"'s
// x-height band is centered in the pill with its baseline on a whole pixel,
// then everything (the pill included, tested analytically) is averaged 8x8
// down to the output size.
public static class LogoSnap
{
    const int K = 8;

    static byte[] Alpha(Bitmap b)
    {
        var data = b.LockBits(new Rectangle(0, 0, b.Width, b.Height), ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        var raw = new byte[data.Stride * b.Height];
        System.Runtime.InteropServices.Marshal.Copy(data.Scan0, raw, 0, raw.Length);
        b.UnlockBits(data);
        var a = new byte[b.Width * b.Height];
        for (int y = 0; y < b.Height; y++)
            for (int x = 0; x < b.Width; x++)
                a[y * b.Width + x] = raw[y * data.Stride + x * 4 + 3];
        return a;
    }

    public static string Build(string[] glyphPngs, string outPng, double padSide, int height)
    {
        var glyphs = new List<byte[]>();
        int sw = 0, sh = 0;
        foreach (var path in glyphPngs)
        {
            var b = new Bitmap(path);
            sw = b.Width; sh = b.Height;
            glyphs.Add(Alpha(b));
            b.Dispose();
        }
        // Ink box of each letter.
        var left = new int[glyphs.Count]; var right = new int[glyphs.Count];
        var top = new int[glyphs.Count]; var bottom = new int[glyphs.Count];
        for (int g = 0; g < glyphs.Count; g++)
        {
            left[g] = sw; right[g] = -1; top[g] = sh; bottom[g] = -1;
            for (int y = 0; y < sh; y++)
                for (int x = 0; x < sw; x++)
                    if (glyphs[g][y * sw + x] > 0)
                    {
                        left[g] = Math.Min(left[g], x); right[g] = Math.Max(right[g], x);
                        top[g] = Math.Min(top[g], y); bottom[g] = Math.Max(bottom[g], y);
                    }
        }
        int padK = (int)Math.Round(padSide) * K;
        // Only the letters with a straight stem on the left (n, i, r: 0, 1, 3)
        // go on whole pixels; t sits halfway between its neighbours' moves and
        // a follows r. The word's starting offset is the one that changes the spacing least.
        Func<int, double[]> shiftsFor = f =>
        {
            var s = new double[glyphs.Count];
            Func<int, double> snap = g => { int p = left[g] - left[0] + f; return Math.Round(p / (double)K, MidpointRounding.AwayFromZero) * K - p; };
            s[0] = snap(0); s[3] = snap(3);
            // The i on the pixel nearer the n: tighter, never wider.
            int pi = left[1] - left[0] + f;
            s[1] = Math.Floor(pi / (double)K) * K - pi;
            s[2] = Math.Round((s[1] + s[3]) / 2.0);
            s[4] = s[3];
            return s;
        };
        int bestF = 0; double bestCost = double.MaxValue;
        for (int f = 0; f < K; f++)
        {
            var s = shiftsFor(f);
            double cost = 0;
            for (int g = 1; g < glyphs.Count; g++) cost += (s[g] - s[g - 1]) * (s[g] - s[g - 1]);
            if (cost < bestCost) { bestCost = cost; bestF = f; }
        }
        var shift = shiftsFor(bestF);
        var targets = new int[glyphs.Count];
        int textW = 0;
        for (int g = 0; g < glyphs.Count; g++)
        {
            targets[g] = padK + left[g] - left[0] + (int)shift[g];
            textW = Math.Max(textW, targets[g] + right[g] - left[g] + 1);
        }
        // The n sits on a whole pixel too: move the word so it does.
        int lead = ((targets[0] % K) + K) % K;
        for (int g = 0; g < glyphs.Count; g++) targets[g] -= lead;
        textW -= lead;
        int width = (int)Math.Ceiling((textW + padK) / (double)K);
        int H = height * K, W = width * K;
        // The n's x-height band (its top includes the arch's overshoot).
        int inkTop = top[0], inkBottom = bottom[0];
        double band = (inkTop + inkBottom + 1) / 2.0, center = H / 2.0;
        int dy = 0; double bestErr = double.MaxValue;
        for (int c = (int)(center - band) - 2 * K; c <= (int)(center - band) + 2 * K; c++)
        {
            if (((bottom[0] + 1 + c) % K + K) % K != 0) continue;
            double err = Math.Abs(band + c - center);
            if (err < bestErr) { bestErr = err; dy = c; }
        }
        var text = new byte[W * H];
        for (int g = 0; g < glyphs.Count; g++)
            for (int y = top[g]; y <= bottom[g]; y++)
            {
                int ty = y + dy;
                if (ty < 0 || ty >= H) continue;
                for (int x = left[g]; x <= right[g]; x++)
                {
                    int tx = targets[g] + x - left[g];
                    byte v = glyphs[g][y * sw + x];
                    if (tx >= 0 && tx < W && v > text[ty * W + tx]) text[ty * W + tx] = v;
                }
            }
        // The pill: a capsule W x H (8x units), sampled at sub-pixel centers.
        double r = H / 2.0;
        var outBmp = new Bitmap(width, height, PixelFormat.Format32bppArgb);
        const double br = 0x00, bg = 0x47, bb = 0xFF;
        for (int oy = 0; oy < height; oy++)
            for (int ox = 0; ox < width; ox++)
            {
                double sa = 0, sr = 0, sg = 0, sb = 0;
                for (int j = 0; j < K; j++)
                    for (int i = 0; i < K; i++)
                    {
                        double px = ox * K + i + 0.5, py = oy * K + j + 0.5;
                        double cx = Math.Max(r, Math.Min(W - r, px));
                        double dx = px - cx, dyy = py - r;
                        if (dx * dx + dyy * dyy > r * r) continue;
                        double t = text[(oy * K + j) * W + ox * K + i] / 255.0;
                        sa += 1;
                        sr += br * (1 - t) + 255 * t;
                        sg += bg * (1 - t) + 255 * t;
                        sb += bb * (1 - t) + 255 * t;
                    }
                if (sa == 0) { outBmp.SetPixel(ox, oy, Color.Transparent); continue; }
                outBmp.SetPixel(ox, oy, Color.FromArgb((int)Math.Round(255 * sa / (K * K)),
                    (int)Math.Round(sr / sa), (int)Math.Round(sg / sa), (int)Math.Round(sb / sa)));
            }
        outBmp.Save(outPng, ImageFormat.Png);
        outBmp.Dispose();
        var gaps = new List<string>();
        for (int g = 1; g < glyphs.Count; g++)
            gaps.Add(((targets[g] - (targets[g - 1] + right[g - 1] - left[g - 1] + 1)) / (double)K).ToString("0.00", System.Globalization.CultureInfo.InvariantCulture));
        return string.Format("{0}x{1}, baseline shift {2}, gaps {3}", width, height, dy, string.Join(" ", gaps));
    }
}
