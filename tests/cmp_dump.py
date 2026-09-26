#!/usr/bin/env python3
"""Compare two screen dumps (pyte vs tns --dump-screen) cell by cell.
usage: cmp_dump.py COLSxROWS A.txt B.txt"""
import sys
cols, rows = map(int, sys.argv[1].split("x"))
a = open(sys.argv[2]).read().split("\n"); b = open(sys.argv[3]).read().split("\n")
print("cursor:", a[0], "|", b[0])
n = 0
for i in range(1, cols * rows + 1):
    if a[i] != b[i]:
        n += 1
        if n <= 15:
            y, x = divmod(i - 1, cols)
            print("row %d col %d: pyte=%r rust=%r" % (y, x, a[i].split("\t"), b[i].split("\t")))
print(n, "cells differ")
