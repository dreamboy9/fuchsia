#!/usr/bin/env python3.8
# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import sys

def main():
    parser = argparse.ArgumentParser(description='Make empty ffx environment')
    parser.add_argument("file", type=argparse.FileType('w'))
    args = parser.parse_args()

    ffx_env = {}
    json.dump(ffx_env, args.file)

if __name__ == '__main__':
    sys.exit(main())

