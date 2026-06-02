#!/usr/bin/env python
"""Create a simple neural synth model for testing Tutti."""

import torch
import torch.nn as nn

class SimpleSynth(nn.Module):
    def __init__(self):
        super().__init__()
        self.harmonic_net = nn.Sequential(
            nn.Linear(128, 256),
            nn.ReLU(),
            nn.Linear(256, 256),
            nn.ReLU(),
            nn.Linear(256, 64)
        )
        self.filter_net = nn.Sequential(
            nn.Linear(128, 128),
            nn.ReLU(),
            nn.Linear(128, 64)
        )
        self.output_proj = nn.Linear(128, 2)

    def forward(self, x):
        harmonics = self.harmonic_net(x)
        filters = self.filter_net(x)
        combined = torch.cat([harmonics, filters], dim=1)
        return self.output_proj(combined)

if __name__ == "__main__":
    model = SimpleSynth()
    model.eval()

    dummy_input = torch.randn(1, 128)

    torch.onnx.export(
        model,
        dummy_input,
        "simple_synth.onnx",
        input_names=['input'],
        output_names=['output'],
        dynamic_axes={'input': {0: 'batch'}, 'output': {0: 'batch'}},
        opset_version=16
    )

    print("✓ Created simple_synth.onnx")
    print()
    print("Convert to Burn format:")
    print("  onnx2burn simple_synth.onnx simple_synth.mpk")
