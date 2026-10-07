import React, { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { SettingsGroup } from "../ui/SettingsGroup";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { Slider } from "../ui/Slider";
import { SettingContainer } from "../ui/SettingContainer";
import { Textarea } from "../ui/Textarea";
import { ShortcutInput } from "./ShortcutInput";
import { useSettings } from "../../hooks/useSettings";

/**
 * Windows hold-Space dictation. The section is only mounted on Windows;
 * other platforms keep the stored settings and do not show these controls.
 */
export const SpacebarDictation: React.FC = () => {
  const { t } = useTranslation();
  const { getSetting, updateSetting, resetSetting, isUpdating } = useSettings();

  const enabled = getSetting("spacebar_dictation_enabled") ?? true;
  const textFieldOnly = getSetting("spacebar_text_field_only") ?? true;
  const blockFullscreen = getSetting("spacebar_block_fullscreen") ?? true;
  const caretIndicator = getSetting("spacebar_caret_indicator") ?? true;
  const threshold = getSetting("spacebar_hold_threshold_ms") ?? 350;
  const storedExcluded = (getSetting("spacebar_excluded_apps") ?? []).join(
    "\n",
  );
  const [excludedDraft, setExcludedDraft] = useState(storedExcluded);

  useEffect(() => {
    setExcludedDraft(storedExcluded);
  }, [storedExcluded]);

  return (
    <SettingsGroup
      title={t("settings.spacebar.title")}
      description={t("settings.spacebar.description")}
    >
      <ToggleSwitch
        checked={enabled}
        onChange={(value) => updateSetting("spacebar_dictation_enabled", value)}
        isUpdating={isUpdating("spacebar_dictation_enabled")}
        label={t("settings.spacebar.enabled.label")}
        description={t("settings.spacebar.enabled.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      <Slider
        value={threshold}
        onChange={(value) => updateSetting("spacebar_hold_threshold_ms", value)}
        onReset={() => resetSetting("spacebar_hold_threshold_ms")}
        isResetting={isUpdating("spacebar_hold_threshold_ms")}
        min={50}
        max={2000}
        step={50}
        label={t("settings.spacebar.threshold.label")}
        description={t("settings.spacebar.threshold.description")}
        descriptionMode="tooltip"
        grouped={true}
        formatValue={(value) => `${value}ms`}
      />
      <ToggleSwitch
        checked={textFieldOnly}
        onChange={(value) => updateSetting("spacebar_text_field_only", value)}
        isUpdating={isUpdating("spacebar_text_field_only")}
        label={t("settings.spacebar.textFieldOnly.label")}
        description={t("settings.spacebar.textFieldOnly.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      <ToggleSwitch
        checked={blockFullscreen}
        onChange={(value) => updateSetting("spacebar_block_fullscreen", value)}
        isUpdating={isUpdating("spacebar_block_fullscreen")}
        label={t("settings.spacebar.blockFullscreen.label")}
        description={t("settings.spacebar.blockFullscreen.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      <ToggleSwitch
        checked={caretIndicator}
        onChange={(value) => updateSetting("spacebar_caret_indicator", value)}
        isUpdating={isUpdating("spacebar_caret_indicator")}
        label={t("settings.spacebar.caretIndicator.label")}
        description={t("settings.spacebar.caretIndicator.description")}
        descriptionMode="tooltip"
        grouped={true}
      />
      <SettingContainer
        title={t("settings.spacebar.excluded.label")}
        description={t("settings.spacebar.excluded.description")}
        descriptionMode="tooltip"
        grouped={true}
        layout="stacked"
      >
        <Textarea
          value={excludedDraft}
          variant="compact"
          placeholder={t("settings.spacebar.excluded.placeholder")}
          disabled={isUpdating("spacebar_excluded_apps")}
          onChange={(event) => setExcludedDraft(event.target.value)}
          onBlur={() => {
            if (excludedDraft !== storedExcluded) {
              updateSetting("spacebar_excluded_apps", [excludedDraft]);
            }
          }}
        />
      </SettingContainer>
      <ShortcutInput shortcutId="toggle_spacebar_dictation" grouped={true} />
    </SettingsGroup>
  );
};
