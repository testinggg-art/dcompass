#!/usr/bin/env bash

targetList=(
	"x86_64-unknown-linux-musl"
	"armv7-unknown-linux-musleabi"
	"armv5te-unknown-linux-musleabi"
	"x86_64-pc-windows-gnu"
)

for i in "${targetList[@]}"; do
	cross build --release --locked --target "$i"
done
