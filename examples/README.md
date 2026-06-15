# Colony Cell Examples

## blank
A minimal cell template. Copy this to create a new cell:
```bash
cp -r examples/blank cell-my-agent
```

## running
Start a colony with:
```bash
# Build the cell binary
cd cell && cargo build --release && cd ..

# Start the API server
python3 colony-api.py --port 8820 &

# Run a cycle
./cell/target/release/cell \
  --colony /path/to/colony \
  --manifest manifest.toml
```
