-- Persist NodeSystemInfo (kernel, os, runtime, kubelet version, ...) so
-- `kubectl get nodes -o wide` can show real details. Stored as JSON text.
ALTER TABLE nodes ADD COLUMN node_info TEXT;
