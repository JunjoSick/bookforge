"""Print Unlimited-OCR's serialized SGLang logit processor.

Usage:
    python scripts/unlimited-ocr-logit-processor.py > unlimited-ocr-processor.txt
"""

try:
    from sglang.srt.sampling.custom_logit_processor import (
        DeepseekOCRNoRepeatNGramLogitProcessor,
    )
except ModuleNotFoundError as error:
    if error.name == "sglang" or (error.name and error.name.startswith("sglang.")):
        raise SystemExit(
            "sglang is not installed; activate the Unlimited-OCR SGLang environment "
            "and install the model-card development wheel first"
        ) from error
    raise
except ImportError as error:
    raise SystemExit(
        "the installed sglang build does not provide "
        "DeepseekOCRNoRepeatNGramLogitProcessor; install the development wheel "
        "recommended by baidu/Unlimited-OCR"
    ) from error


print(DeepseekOCRNoRepeatNGramLogitProcessor.to_str())
