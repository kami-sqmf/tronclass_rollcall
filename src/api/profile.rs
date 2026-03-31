//! 使用者資料 Payload、Response 資料結構，以及 /api/profile 呼叫實作

use miette::{IntoDiagnostic, Result, WrapErr};
use serde::{Deserialize, Serialize};
use tracing::{debug, instrument};

// ─── Response 資料結構 ────────────────────────────────────────────────────────

/// `GET /api/profile` — 使用者基本資訊
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UserProfile {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub user_no: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub department: Option<ProfileDepartment>,
    #[serde(default)]
    pub grade: Option<ProfileGrade>,
    #[serde(default)]
    pub klass: Option<ProfileKlass>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileDepartment {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub short_name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileGrade {
    pub id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileKlass {
    pub id: u64,
    pub name: String,
}

// ─── ApiClient /api/profile 方法 ──────────────────────────────────────────────

impl super::ApiClient {
    /// `GET /api/profile` — 取得使用者資料（同時驗證 Session 有效性）
    #[instrument(skip(self))]
    pub async fn get_profile(&self) -> Result<UserProfile> {
        let url = format!("{}/api/profile", self.base_url);
        debug!(url = %url, "GET /api/profile");

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .into_diagnostic()
            .wrap_err("GET /api/profile failed")?;

        self.handle_response(resp).await
    }
}

// ─── 測試 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 模擬實際 /api/profile 回應（敏感欄位已匿名化）
    fn sample_profile_json() -> &'static str {
        r#"{
            "id": 999999,
            "name": "倪氏箇賈",
            "user_no": "000000000",
            "role": "Student",
            "email": "000000000@cloud.fju.edu.tw",
            "department": {
                "id": 421,
                "name": "(日)擬人研究學系",
                "short_name": "(日)擬究"
            },
            "grade": { "id": 55, "name": "四年級" },
            "klass": { "id": 1450, "name": "擬究四甲" }
        }"#
    }

    #[test]
    fn test_user_profile_deserialize() {
        let profile: UserProfile = serde_json::from_str(sample_profile_json()).unwrap();
        assert_eq!(profile.id, 999999);
        assert_eq!(profile.name, "倪氏箇賈");
        assert_eq!(profile.user_no, "000000000");
        assert_eq!(profile.role, "Student");
        assert_eq!(profile.email, "000000000@cloud.fju.edu.tw");

        let dept = profile.department.unwrap();
        assert_eq!(dept.id, 421);
        assert_eq!(dept.name, "(日)擬人研究學系");
        assert_eq!(dept.short_name, "(日)擬究");

        assert_eq!(profile.grade.unwrap().name, "四年級");
        assert_eq!(profile.klass.unwrap().name, "擬究四甲");
    }

    #[test]
    fn test_user_profile_deserialize_ignores_unknown_fields() {
        // 確保 API 新增欄位不會導致解析失敗
        let json = r#"{
            "id": 1,
            "name": "測試學生",
            "some_future_field": true,
            "nested_future": { "foo": "bar" }
        }"#;
        let profile: UserProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile.name, "測試學生");
        assert!(profile.department.is_none());
    }
}
