#!/usr/bin/env bash

n=${1:-10}

for ((i=1; i<=n; i++)); do
    echo "$i"
    sleep 1
done

echo "Hello World" > result.txt
echo "Done"
