import numpy as np
import tensorflow as tf
from tensorflow.keras.models import Sequential
from tensorflow.keras.layers import Dense, Activation
import onnxmltools

import tract

# Define a simple demo model and training data
model = Sequential([
    Dense(32, activation='relu', input_dim=100,  name='main_input'),
    Dense(1, activation='sigmoid', name="dense_1"),
])
model.compile(optimizer='rmsprop',
              loss='binary_crossentropy',
              metrics=['accuracy'])

data = np.random.random((1000, 100))
labels = np.random.randint(2, size=(1000, 1))

# Train the model, iterating on the data in batches of 32 samples
model.fit(data, labels, epochs=10, batch_size=32)

# Save the model in ONNX format to pass to tract
onnx_model = onnxmltools.convert_keras(model)

onnxmltools.utils.save_model(onnx_model, 'example.onnx')

# Generate a demo input, and run the model in Tensorflow
input = np.random.random((1,100)).astype(np.float32)
tf_output = model.predict(input)

# Run the model in tract and check output against TensorFlow
tract_output = tract.onnx().model_for_path("example.onnx")\
        .into_optimized().into_runnable().run([input])[0].to_numpy()
assert(np.allclose(tf_output, tract_output))

# Save input and reference output for Rust demo
np.savez("io.npz", input=input, output=tf_output)
