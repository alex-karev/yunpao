#!/usr/bin/env bash

for ((i=1; i<=10; i++)); do
    echo "$i"
    sleep 1
done

echo "Hello World" > result.txt
echo "Done"
