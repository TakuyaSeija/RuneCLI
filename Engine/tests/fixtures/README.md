# Classifier/compact fixtures

The real-model runner generates a small PNG in its isolated project at test time.
It contains only synthetic colour pixels, never an owner screenshot or secret.
Credentials are stored only in the runner's isolated encrypted home and removed
when it finishes. The ignored live test requires explicit provider/model selection.
