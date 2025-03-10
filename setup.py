from setuptools import setup, find_packages

# Read the contents of your README.md file for the project description
with open("README.md", "r", encoding="utf-8") as readme_file:
    long_description = readme_file.read()

setup(
    name="hanzo-operator",
    version="0.0.2",
    packages=find_packages(),
    install_requires=[
        "openai>=1.12.0",
        "Pillow>=10.0.0",
        "PyAutoGUI>=0.9.54",
        "python-dotenv>=1.0.0",
        "easyocr>=1.7.1",
        "google-generativeai>=0.3.2",
        "anthropic>=0.18.1",
        "ollama>=0.1.7",
        "numpy>=1.24.3",
        "matplotlib>=3.7.1",
        "requests>=2.31.0",
        "torch>=2.0.1",
        "torchvision>=0.15.2",
        "ultralytics>=8.0.196"
    ],
    entry_points={
        "console_scripts": [
            "operate=operate.main:main_entry",
        ],
    },
    package_data={
        # Include the file in the operate.models.weights package
        "operate.models.weights": ["best.pt"],
    },
    long_description=long_description,  # Add project description here
    long_description_content_type="text/markdown",  # Specify Markdown format
    author="Hanzo AI",
    author_email="your.email@example.com",
    description="A framework to enable multimodal models to operate a computer",
    url="https://github.com/yourusername/hanzo-operator",
    classifiers=[
        "Programming Language :: Python :: 3",
        "License :: OSI Approved :: MIT License",
        "Operating System :: OS Independent",
    ],
    python_requires=">=3.8",
)
