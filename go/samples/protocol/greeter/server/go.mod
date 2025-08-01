module github.com/Azure/iot-operations-sdks/go/samples/protocol/greeter/server

go 1.24.0

require (
	github.com/Azure/iot-operations-sdks/go/mqtt v0.4.0
	github.com/Azure/iot-operations-sdks/go/protocol v0.4.1
	github.com/Azure/iot-operations-sdks/go/samples/protocol/greeter/envoy v0.0.0
	github.com/lmittmann/tint v1.0.7
)

require (
	github.com/Azure/iot-operations-sdks/go/internal v0.3.0 // indirect
	github.com/VividCortex/ewma v1.2.0 // indirect
	github.com/cheggaaa/pb/v3 v3.1.7 // indirect
	github.com/eclipse/paho.golang v0.22.0 // indirect
	github.com/fatih/color v1.18.0 // indirect
	github.com/fsnotify/fsnotify v1.8.0 // indirect
	github.com/google/uuid v1.6.0 // indirect
	github.com/iancoleman/strcase v0.3.0 // indirect
	github.com/mattn/go-colorable v0.1.14 // indirect
	github.com/mattn/go-isatty v0.0.20 // indirect
	github.com/mattn/go-runewidth v0.0.16 // indirect
	github.com/princjef/mageutil v1.0.0 // indirect
	github.com/relvacode/iso8601 v1.6.0 // indirect
	github.com/rivo/uniseg v0.4.7 // indirect
	github.com/sosodev/duration v1.3.1 // indirect
	golang.org/x/crypto v0.36.0 // indirect
	golang.org/x/sys v0.31.0 // indirect
)

replace github.com/Azure/iot-operations-sdks/go/samples/protocol/greeter/envoy => ../envoy
