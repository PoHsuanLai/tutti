"""Generate a tiny ONNX model for testing burn-import integration.

Run: python3 gen_test_model.py
Output: tiny_effect.onnx, reference_input.json, reference_output.json
"""
import json
import math
import os

import torch
import torch.nn as nn


class TinyEffect(nn.Module):
    def __init__(self):
        super().__init__()
        self.net = nn.Sequential(
            nn.Linear(128, 64),
            nn.ReLU(),
            nn.Linear(64, 128),
        )

    def forward(self, x):
        return self.net(x)


# Deterministic weights
torch.manual_seed(42)
model = TinyEffect()
model.eval()

out_dir = os.path.dirname(__file__)

# Export ONNX
torch.onnx.export(
    model,
    torch.randn(1, 128),
    os.path.join(out_dir, "tiny_effect.onnx"),
    input_names=["input"],
    output_names=["output"],
    dynamic_axes={"input": {0: "batch"}, "output": {0: "batch"}},
    opset_version=16,
)
print("Exported: tiny_effect.onnx")

# Generate reference input: sin(i/128) for i in 0..128
ref_input = [math.sin(i / 128.0) for i in range(128)]
ref_tensor = torch.tensor([ref_input], dtype=torch.float32)

with torch.no_grad():
    ref_output = model(ref_tensor)

ref_output_list = ref_output[0].tolist()

with open(os.path.join(out_dir, "reference_input.json"), "w") as f:
    json.dump(ref_input, f)

with open(os.path.join(out_dir, "reference_output.json"), "w") as f:
    json.dump(ref_output_list, f)

print(f"Reference input (first 4):  {ref_input[:4]}")
print(f"Reference output (first 4): {ref_output_list[:4]}")
print("Saved: reference_input.json, reference_output.json")
