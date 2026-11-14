"""Convert official Real-ESRGAN PyTorch weights (.pth) to ONNX for Stereowright.

Network architectures below are transcribed from the official code
(xinntao/Real-ESRGAN and XPixelGroup/BasicSR, BSD-3-Clause). The weights are
loaded with weights_only=True (no pickle code execution) and strictly, so any
mismatch with the official checkpoint fails loudly. The exported model is then
checked against PyTorch on a differently sized input.

Usage: python -I export_realesrgan.py --arch x4plus --weights W.pth --out M.onnx
"""

import argparse
import hashlib
import sys

import numpy as np
import torch
import torch.nn.functional as F
from torch import nn


class ResidualDenseBlock(nn.Module):
    def __init__(self, num_feat=64, num_grow_ch=32):
        super().__init__()
        self.conv1 = nn.Conv2d(num_feat, num_grow_ch, 3, 1, 1)
        self.conv2 = nn.Conv2d(num_feat + num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv3 = nn.Conv2d(num_feat + 2 * num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv4 = nn.Conv2d(num_feat + 3 * num_grow_ch, num_grow_ch, 3, 1, 1)
        self.conv5 = nn.Conv2d(num_feat + 4 * num_grow_ch, num_feat, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(negative_slope=0.2)

    def forward(self, x):
        x1 = self.lrelu(self.conv1(x))
        x2 = self.lrelu(self.conv2(torch.cat((x, x1), 1)))
        x3 = self.lrelu(self.conv3(torch.cat((x, x1, x2), 1)))
        x4 = self.lrelu(self.conv4(torch.cat((x, x1, x2, x3), 1)))
        x5 = self.conv5(torch.cat((x, x1, x2, x3, x4), 1))
        return x5 * 0.2 + x


class RRDB(nn.Module):
    def __init__(self, num_feat, num_grow_ch=32):
        super().__init__()
        self.rdb1 = ResidualDenseBlock(num_feat, num_grow_ch)
        self.rdb2 = ResidualDenseBlock(num_feat, num_grow_ch)
        self.rdb3 = ResidualDenseBlock(num_feat, num_grow_ch)

    def forward(self, x):
        out = self.rdb3(self.rdb2(self.rdb1(x)))
        return out * 0.2 + x


class RRDBNet(nn.Module):
    def __init__(self, num_in_ch=3, num_out_ch=3, scale=4, num_feat=64, num_block=23, num_grow_ch=32):
        super().__init__()
        self.scale = scale
        if scale == 2:
            num_in_ch = num_in_ch * 4
        elif scale == 1:
            num_in_ch = num_in_ch * 16
        self.conv_first = nn.Conv2d(num_in_ch, num_feat, 3, 1, 1)
        self.body = nn.Sequential(*[RRDB(num_feat, num_grow_ch) for _ in range(num_block)])
        self.conv_body = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_up1 = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_up2 = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_hr = nn.Conv2d(num_feat, num_feat, 3, 1, 1)
        self.conv_last = nn.Conv2d(num_feat, num_out_ch, 3, 1, 1)
        self.lrelu = nn.LeakyReLU(negative_slope=0.2)

    def forward(self, x):
        if self.scale == 2:
            feat = F.pixel_unshuffle(x, 2)
        elif self.scale == 1:
            feat = F.pixel_unshuffle(x, 4)
        else:
            feat = x
        feat = self.conv_first(feat)
        feat = feat + self.conv_body(self.body(feat))
        feat = self.lrelu(self.conv_up1(F.interpolate(feat, scale_factor=2, mode="nearest")))
        feat = self.lrelu(self.conv_up2(F.interpolate(feat, scale_factor=2, mode="nearest")))
        return self.conv_last(self.lrelu(self.conv_hr(feat)))


class SRVGGNetCompact(nn.Module):
    def __init__(self, num_in_ch=3, num_out_ch=3, num_feat=64, num_conv=32, upscale=4):
        super().__init__()
        self.upscale = upscale
        body = [nn.Conv2d(num_in_ch, num_feat, 3, 1, 1), nn.PReLU(num_parameters=num_feat)]
        for _ in range(num_conv):
            body += [nn.Conv2d(num_feat, num_feat, 3, 1, 1), nn.PReLU(num_parameters=num_feat)]
        body.append(nn.Conv2d(num_feat, num_out_ch * upscale * upscale, 3, 1, 1))
        self.body = nn.ModuleList(body)
        self.upsampler = nn.PixelShuffle(upscale)

    def forward(self, x):
        out = x
        for layer in self.body:
            out = layer(out)
        out = self.upsampler(out)
        return out + F.interpolate(x, scale_factor=self.upscale, mode="nearest")


ARCHS = {
    "x4plus": lambda: RRDBNet(scale=4),
    "x2plus": lambda: RRDBNet(scale=2),
    "general-x4v3": lambda: SRVGGNetCompact(num_conv=32, upscale=4),
}


def export(net, out_path, multiple):
    x = torch.rand(1, 3, 64, 64)
    try:
        torch.onnx.export(
            net, (x,), out_path,
            input_names=["input"], output_names=["output"],
            dynamic_axes={"input": {2: "h", 3: "w"}, "output": {2: "oh", 3: "ow"}},
            opset_version=17, dynamo=False,
        )
        return "torchscript exporter"
    except Exception as e:  # legacy exporter removed in this torch: use the dynamo one
        print(f"legacy exporter unavailable ({e.__class__.__name__}); using dynamo", file=sys.stderr)
    from torch.export import Dim
    h, w = Dim("h", min=4, max=2048), Dim("w", min=4, max=2048)
    shapes = ({2: h * multiple, 3: w * multiple},) if multiple > 1 else ({2: h, 3: w},)
    prog = torch.onnx.export(net, (x,), dynamo=True, dynamic_shapes=shapes,
                             input_names=["input"], output_names=["output"], opset_version=18)
    prog.save(out_path)
    return "dynamo exporter"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arch", choices=sorted(ARCHS), required=True)
    ap.add_argument("--weights", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()

    net = ARCHS[a.arch]()
    ckpt = torch.load(a.weights, map_location="cpu", weights_only=True)
    state = ckpt.get("params_ema", ckpt.get("params", ckpt))
    net.load_state_dict(state, strict=True)
    net.eval()

    multiple = 2 if a.arch == "x2plus" else 1
    with torch.no_grad():
        how = export(net, a.out, multiple)

        import onnxruntime as ort
        probe = torch.rand(1, 3, 40 * multiple, 56 * multiple)  # a size never seen during export
        ref = net(probe).numpy()
        sess = ort.InferenceSession(a.out, providers=["CPUExecutionProvider"])
        got = sess.run(None, {sess.get_inputs()[0].name: probe.numpy()})[0]
    err = float(np.abs(ref - got).max())
    if got.shape != ref.shape or err > 2e-3:
        sys.exit(f"verification FAILED: shape {got.shape} vs {ref.shape}, max error {err}")
    digest = hashlib.sha256(open(a.out, "rb").read()).hexdigest()
    print(f"{a.arch}: ok via {how}; output {got.shape}, max |onnx - torch| = {err:.2e}; sha256 {digest}")


if __name__ == "__main__":
    main()
