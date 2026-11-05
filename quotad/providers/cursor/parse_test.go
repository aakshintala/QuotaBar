package cursor

import (
	"context"
	"math"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"quotad/monitor"
)

func fixture(t *testing.T, name string) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

func timeLimitQuota(snap monitor.Snapshot, name string) *monitor.Quota {
	for i, q := range snap.Quotas {
		if q.Type.Kind == monitor.KindTime && q.Type.Name == name {
			return &snap.Quotas[i]
		}
	}
	return nil
}

func TestParseRealUltraPlanResponse(t *testing.T) {
	snap, err := parseUsageSummary(fixture(t, "usage-summary.json"))
	if err != nil {
		t.Fatal(err)
	}
	if snap.ProviderID != "cursor" {
		t.Errorf("provider = %q", snap.ProviderID)
	}
	if len(snap.Quotas) != 1 {
		t.Fatalf("quotas = %d, want 1", len(snap.Quotas))
	}
	if snap.Tier == nil || snap.Tier.Badge != "ULTRA" {
		t.Errorf("tier = %+v, want ULTRA", snap.Tier)
	}
	q := snap.Quotas[0]
	if q.Type.Name != "Monthly" {
		t.Errorf("type = %q, want Monthly", q.Type.Name)
	}
	if math.Abs(*q.PercentRemaining-99.185) > 0.01 {
		t.Errorf("remaining = %v, want ~99.185", *q.PercentRemaining)
	}
	if *q.UnitsUsed != 326 || *q.UnitsLimit != 40000 {
		t.Errorf("units = %d/%d, want 326/40000", *q.UnitsUsed, *q.UnitsLimit)
	}
	if q.ResetsAt == nil {
		t.Error("resetsAt is nil")
	}
}

func TestParseProPlan(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "billingCycleEnd": "2025-02-01T00:00:00Z",
		"isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 123, "limit": 500, "remaining": 377},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Fatalf("quotas = %d, want 1", len(snap.Quotas))
	}
	if math.Abs(*snap.Quotas[0].PercentRemaining-75.4) > 0.1 {
		t.Errorf("remaining = %v, want ~75.4", *snap.Quotas[0].PercentRemaining)
	}
	if snap.Quotas[0].ResetsAt == nil {
		t.Error("resetsAt is nil")
	}
}

func TestParsePlanWithOnDemand(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 400, "limit": 500, "remaining": 100},
			"onDemand": {"enabled": true, "used": 25, "limit": 100, "remaining": 75}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 2 {
		t.Fatalf("quotas = %d, want 2", len(snap.Quotas))
	}
	plan := timeLimitQuota(snap, "Monthly")
	if plan == nil || math.Abs(*plan.PercentRemaining-20.0) > 0.1 {
		t.Errorf("plan remaining = %+v, want ~20", plan)
	}
	od := timeLimitQuota(snap, "On-Demand")
	if od == nil {
		t.Fatal("missing On-Demand quota")
	}
	if math.Abs(*od.PercentRemaining-75.0) > 0.1 || *od.UnitsUsed != 25 || *od.UnitsLimit != 100 {
		t.Errorf("on-demand = %v %d/%d", *od.PercentRemaining, *od.UnitsUsed, *od.UnitsLimit)
	}
}

func TestParseDepletedPlan(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 500, "limit": 500, "remaining": 0},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if *snap.Quotas[0].PercentRemaining != 0 {
		t.Errorf("remaining = %v, want 0", *snap.Quotas[0].PercentRemaining)
	}
}

// Regression port: bonus credits max out used/limit while breakdown.total
// holds the real capacity. Percent must come from totalPercentUsed, not 0.
func TestParseBonusCreditsNotDepleted(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"billingCycleStart": "2026-06-25T03:47:17.000Z",
		"billingCycleEnd": "2026-07-25T03:47:17.000Z",
		"membershipType": "pro", "limitType": "user", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 2000, "limit": 2000, "remaining": 0,
				"breakdown": {"included": 2000, "bonus": 7770, "total": 9770},
				"totalPercentUsed": 28.32},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		},
		"teamUsage": {}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Fatalf("quotas = %d, want 1", len(snap.Quotas))
	}
	q := snap.Quotas[0]
	if math.Abs(*q.PercentRemaining-71.68) > 0.1 {
		t.Errorf("remaining = %v, want ~71.68", *q.PercentRemaining)
	}
	if *q.UnitsUsed != 2767 || *q.UnitsLimit != 9770 {
		t.Errorf("units = %d/%d, want 2767/9770", *q.UnitsUsed, *q.UnitsLimit)
	}
}

func TestParseOverLimitClampsToZero(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 550, "limit": 500, "remaining": -50},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if *snap.Quotas[0].PercentRemaining != 0 {
		t.Errorf("remaining = %v, want 0", *snap.Quotas[0].PercentRemaining)
	}
}

func TestParseUnlimitedPlan(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "business", "isUnlimited": true,
		"individualUsage": {
			"plan": {"enabled": false},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Fatalf("quotas = %d, want 1", len(snap.Quotas))
	}
	q := snap.Quotas[0]
	if *q.PercentRemaining != 100 {
		t.Errorf("remaining = %v, want 100", *q.PercentRemaining)
	}
	if q.UnitsUsed != nil || q.UnitsLimit != nil {
		t.Error("unlimited quota must not carry unit counts")
	}
}

func TestParseFreePlan(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "free", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 30, "limit": 50, "remaining": 20},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if snap.Tier == nil || snap.Tier.Badge != "FREE" {
		t.Errorf("tier = %+v, want FREE", snap.Tier)
	}
	if math.Abs(*snap.Quotas[0].PercentRemaining-40.0) > 0.1 {
		t.Errorf("remaining = %v, want 40", *snap.Quotas[0].PercentRemaining)
	}
}

func TestParseEmptyMembershipOmitsTier(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 10, "limit": 100},
			"onDemand": {"enabled": false, "used": 0, "limit": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if snap.Tier != nil {
		t.Errorf("tier = %+v, want nil for an empty membershipType", snap.Tier)
	}
}

func TestParseMissingMembershipDefaultsUnknownTier(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 10, "limit": 100},
			"onDemand": {"enabled": false, "used": 0, "limit": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if snap.Tier == nil || snap.Tier.Badge != "UNKNOWN" {
		t.Errorf("tier = %+v, want UNKNOWN when membershipType is missing", snap.Tier)
	}
}

func TestParseEnterpriseTeamPlan(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"billingCycleStart": "2026-03-01T00:00:00.000Z",
		"billingCycleEnd": "2026-04-01T00:00:00.000Z",
		"membershipType": "enterprise", "limitType": "team", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 0, "limit": 0, "remaining": 0,
				"breakdown": {"included": 0, "bonus": 300, "total": 300},
				"totalPercentUsed": 6.9},
			"onDemand": {"enabled": false, "used": 0, "limit": 0, "remaining": 0}
		},
		"teamUsage": {
			"onDemand": {"enabled": true, "used": 0, "limit": 10000, "remaining": 10000}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 2 {
		t.Fatalf("quotas = %d, want 2", len(snap.Quotas))
	}
	ind := timeLimitQuota(snap, "Monthly")
	if ind == nil || math.Abs(*ind.PercentRemaining-93.1) > 0.5 {
		t.Errorf("individual remaining = %+v, want ~93.1", ind)
	}
	team := timeLimitQuota(snap, "Team")
	if team == nil {
		t.Fatal("missing Team quota")
	}
	if *team.PercentRemaining != 100 || *team.UnitsUsed != 0 || *team.UnitsLimit != 10000 {
		t.Errorf("team = %v %d/%d", *team.PercentRemaining, *team.UnitsUsed, *team.UnitsLimit)
	}
}

func TestParseEnterpriseBreakdownFallback(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "enterprise", "limitType": "team", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 0, "limit": 0, "remaining": 0,
				"breakdown": {"included": 0, "bonus": 184, "total": 184},
				"totalPercentUsed": 50.0},
			"onDemand": {"enabled": false, "used": 0, "limit": 0, "remaining": 0}
		},
		"teamUsage": {
			"onDemand": {"enabled": false, "used": 0, "limit": 0, "remaining": 0}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Fatalf("quotas = %d, want 1", len(snap.Quotas))
	}
	q := snap.Quotas[0]
	if math.Abs(*q.PercentRemaining-50.0) > 0.5 {
		t.Errorf("remaining = %v, want 50", *q.PercentRemaining)
	}
	if *q.UnitsUsed != 92 || *q.UnitsLimit != 184 {
		t.Errorf("units = %d/%d, want 92/184", *q.UnitsUsed, *q.UnitsLimit)
	}
}

func TestParseTeamUsageIgnoredWithoutTeamLimitType(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "limitType": "user", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 10, "limit": 100},
			"onDemand": {"enabled": false, "used": 0, "limit": null}
		},
		"teamUsage": {
			"onDemand": {"enabled": true, "used": 0, "limit": 10000, "remaining": 10000}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if len(snap.Quotas) != 1 {
		t.Errorf("quotas = %d, want 1 (team ignored)", len(snap.Quotas))
	}
}

func TestParseInvalidBodiesError(t *testing.T) {
	for _, body := range []string{
		"{}",
		"not json",
		`{"membershipType": "pro", "isUnlimited": false}`,
	} {
		if _, err := parseUsageSummary([]byte(body)); err == nil {
			t.Errorf("body %q: expected an error", body)
		}
	}
}

func TestParseBillingCycleEndForms(t *testing.T) {
	for _, end := range []string{"2025-03-01T00:00:00.000Z", "2025-03-01T00:00:00Z"} {
		snap, err := parseUsageSummary([]byte(`{
			"membershipType": "pro", "isUnlimited": false,
			"billingCycleEnd": "` + end + `",
			"individualUsage": {
				"plan": {"enabled": true, "used": 100, "limit": 500, "remaining": 400},
				"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
			}}`))
		if err != nil {
			t.Fatal(err)
		}
		if snap.Quotas[0].ResetsAt == nil {
			t.Errorf("end %q: resetsAt is nil", end)
		}
	}
}

func TestParseDoubleCounts(t *testing.T) {
	snap, err := parseUsageSummary([]byte(`{
		"membershipType": "pro", "isUnlimited": false,
		"individualUsage": {
			"plan": {"enabled": true, "used": 123.0, "limit": 500.0, "remaining": 377.0},
			"onDemand": {"enabled": false, "used": 0, "limit": null, "remaining": null}
		}}`))
	if err != nil {
		t.Fatal(err)
	}
	if math.Abs(*snap.Quotas[0].PercentRemaining-75.4) > 0.1 {
		t.Errorf("remaining = %v, want ~75.4", *snap.Quotas[0].PercentRemaining)
	}
}

func TestDecodeJWT(t *testing.T) {
	valid := []struct{ token, sub string }{
		{pipeJWT(`{"sub":"user_abc123","iat":1234567890}`), "user_abc123"},
		{pipeJWT(`{"sub":"github|user_01ABC","type":"session"}`), "github|user_01ABC"},
		{pipeJWT(`{"sub":"u1"}`), "u1"},
	}
	for _, tc := range valid {
		sub, _, err := decodeJWT(tc.token)
		if err != nil {
			t.Errorf("token %q: %v", tc.token, err)
			continue
		}
		if sub != tc.sub {
			t.Errorf("sub = %q, want %q", sub, tc.sub)
		}
	}
	for _, token := range []string{"not-a-jwt", pipeJWT(`{"iat":123}`)} {
		if _, _, err := decodeJWT(token); err == nil {
			t.Errorf("token %q: expected an error", token)
		}
	}
}

func TestCookieUserID(t *testing.T) {
	for sub, want := range map[string]string{
		"github|user_01ABC": "user_01ABC",
		"a|b|c":             "c",
		"user_abc123":       "user_abc123",
	} {
		got, err := cookieUserID(sub)
		if err != nil || got != want {
			t.Errorf("sub %q: got %q, %v; want %q", sub, got, err, want)
		}
	}
	for _, sub := range []string{"", "user|bad;chars", "a|b c", "x|y/z"} {
		if id, err := cookieUserID(sub); err == nil {
			t.Errorf("sub %q: expected rejection, got %q", sub, id)
		}
	}
}

func TestErrorStringsCarryNoSecrets(t *testing.T) {
	fake := "fake-secret-value-ABC123"
	if _, _, err := decodeJWT(fake); err == nil {
		t.Fatal("expected a decode error for a bogus token")
	} else if strings.Contains(err.Error(), fake) {
		t.Error("decode error echoes the token")
	}
	badSubToken := pipeJWT(`{"sub":"evil|bad;chars"}`)
	sub, _, err := decodeJWT(badSubToken)
	if err != nil {
		t.Fatalf("setup decode failed: %v", err)
	}
	if _, err := cookieUserID(sub); err == nil {
		t.Fatal("expected a user-id error")
	} else if strings.Contains(err.Error(), "bad;chars") || strings.Contains(err.Error(), badSubToken) {
		t.Error("user-id error echoes its input")
	}
	token := testJWT("leakcheck|user_999", futureExp())
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusInternalServerError)
	}))
	defer srv.Close()
	p, home := testProvider(t, srv)
	writeAuthJSON(t, home, token)
	if _, err := p.Fetch(context.Background()); err == nil {
		t.Fatal("expected an HTTP 500 error")
	} else if strings.Contains(err.Error(), token) {
		t.Error("request error echoes the access token")
	}
}
