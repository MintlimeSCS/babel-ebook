import { memo, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ModelParams } from "../types";
import Tooltip from "../components/Tooltip";

interface ModelParamsPageProps {
  modelParams: ModelParams;
  setModelParams: (update: Partial<ModelParams>) => void;
}

type NumericParamKey = "max_input_tokens" | "max_output_tokens" | "temperature";

interface FieldMeta {
  key: NumericParamKey;
  labelKey: string;
  helpKey: string;
  min: number;
  max?: number;
  step?: string;
}

const FIELDS: FieldMeta[] = [
  { key: "max_input_tokens", labelKey: "max_input_tokens", helpKey: "max_input_tokens_help", min: 1 },
  { key: "max_output_tokens", labelKey: "max_output_tokens", helpKey: "max_output_tokens_help", min: 1 },
  { key: "temperature", labelKey: "temperature", helpKey: "temperature_help", min: 0, max: 2, step: "0.1" },
];

function clamp(value: number, min: number, max?: number): number {
  let v = value;
  if (Number.isNaN(v)) return min;
  v = Math.max(min, v);
  if (max !== undefined) v = Math.min(max, v);
  return v;
}

function ModelParamsPage({ modelParams, setModelParams }: ModelParamsPageProps) {
  const { t } = useTranslation();
  const [errors, setErrors] = useState<Record<string, string>>({});

  const validate = (_key: NumericParamKey, raw: number, meta: FieldMeta): string | undefined => {
    if (Number.isNaN(raw)) return t("error_number_required");
    if (raw < meta.min) return t("error_number_min", { min: meta.min });
    if (meta.max !== undefined && raw > meta.max) return t("error_number_max", { max: meta.max });
    return undefined;
  };

  const rows = useMemo(() => {
    const first = FIELDS.slice(0, 2);
    const second = FIELDS.slice(2);
    return [first, second];
  }, []);

  const handleChange = (meta: FieldMeta, value: string) => {
    const raw = value === "" ? Number.NaN : Number(value);
    const error = validate(meta.key, raw, meta);
    setErrors((prev) => ({ ...prev, [meta.key]: error ?? "" }));
    setModelParams({ [meta.key]: clamp(raw, meta.min, meta.max) });
  };

  const handleBlur = (meta: FieldMeta) => {
    setModelParams({ [meta.key]: clamp(modelParams[meta.key], meta.min, meta.max) });
    setErrors((prev) => ({ ...prev, [meta.key]: "" }));
  };

  return (
    <div className="page settings-page">
      <h2>{t("settings_model")}</h2>

      <label><input data-testid="paragraph-merge-enabled" type="checkbox" checked={modelParams.paragraph_merge.enabled}
        onChange={(e) => setModelParams({ paragraph_merge: { ...modelParams.paragraph_merge, enabled: e.target.checked } })} />合併相鄰短段落</label>
      <p>只合併同一文件中相鄰的純文字段落；連結、註解、內嵌標籤與啟用潤色的內容沿用原流程。回應驗證失敗會逐段重譯。</p>
      <div className="row">
        {([ ["max_paragraph_tokens", "每段最多 Token", 1, 256], ["max_paragraphs", "每組最多段落", 2, 8] ] as const).map(([key, label, min, max]) => (
          <label key={key}>{label}<input type="number" min={min} max={max} value={modelParams.paragraph_merge[key]}
            onChange={(e) => setModelParams({ paragraph_merge: { ...modelParams.paragraph_merge, [key]: Math.trunc(clamp(Number(e.target.value), min, max)) } })} /></label>
        ))}
      </div>
      <p>API 費用估計單價（USD／百萬 Token）。請按所選模型填入；留空則不估價。僅套用於 OpenAI 翻譯。</p>
      <p>單價綁定模型：{modelParams.usage_prices.model || "尚未設定"}</p>
      <div className="row">
        {([ ["input", "一般輸入"], ["cached_input", "API 快取輸入"], ["output", "輸出"] ] as const).map(([key, label]) => (
          <label key={key}>{label}<input data-testid={`usage-price-${key}`} type="number" min="0" step="0.01"
            value={modelParams.usage_prices[key] ?? ""}
            onChange={(e) => {
              const value = e.target.value === "" ? null : Number(e.target.value);
              if (value !== null && (!Number.isFinite(value) || value < 0)) return;
              setModelParams({ usage_prices: { ...modelParams.usage_prices, model: modelParams.model, [key]: value } });
            }} /></label>
        ))}
      </div>
      {rows.map((row, rowIndex) => (
        <div className="row" key={rowIndex}>
          {row.map((meta) => (
            <label key={meta.key}>
              <span className="field-row">
                {t(meta.labelKey)}
                <Tooltip content={t(meta.helpKey)}>
                  <span className="field-info" aria-hidden="true">ⓘ</span>
                </Tooltip>
              </span>
              <input
                type="number"
                min={meta.min}
                max={meta.max}
                step={meta.step}
                value={modelParams[meta.key]}
                onChange={(e) => handleChange(meta, e.target.value)}
                onBlur={() => handleBlur(meta)}
                aria-invalid={!!errors[meta.key]}
                aria-errormessage={errors[meta.key] ? `error-${meta.key}` : undefined}
              />
              {errors[meta.key] && (
                <span className="inline-error" id={`error-${meta.key}`} role="alert">
                  {errors[meta.key]}
                </span>
              )}
            </label>
          ))}
        </div>
      ))}
    </div>
  );
}

export default memo(ModelParamsPage);
