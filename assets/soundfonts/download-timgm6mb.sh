#!/bin/bash
set -e

if [ -f "TimGM6mb.sf2" ]; then
    echo "✓ TimGM6mb.sf2 already exists"
    exit 0
fi

echo "Downloading TimGM6mb.sf2 (5.7 MB)..."
curl -L -o TimGM6mb.tar.gz "http://http.debian.net/debian/pool/main/t/timgm6mb-soundfont/timgm6mb-soundfont_1.3.orig.tar.gz"
tar -xzf TimGM6mb.tar.gz --strip-components=1 "*/TimGM6mb.sf2"
rm TimGM6mb.tar.gz

echo "✓ Download complete"
