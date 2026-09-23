// Package def is the vocabulary of the generated resource tables: every
// Sylphx Resource the provider manages is a *Resource value in
// internal/generated, and the provider's generic code reads nothing else.
package def

// Kind is the wire type of a field (resource-api-and-clients.md §3.1).
type Kind int

const (
	String Kind = iota
	Bool
	Int32
	Int64 // travels as a JSON string
	Double
	Bytes     // base64 string
	Enum      // wire string, one of Field.Values
	Timestamp // RFC 3339 string
	Duration  // "30s"
	JSON      // Any / Struct / Value: a JSON-encoded string attribute
	Message   // nested object, fields from Field.Msg
	Map       // string keys, values described by Field.Elem
)

// Flag is a field behavior (google.api.field_behavior plus Sylphx options).
type Flag uint

const (
	Required Flag = 1 << iota
	OutputOnly
	Immutable
	InputOnly
	Sensitive
	// Presence: proto3 `optional` or a oneof member; absent means null, not
	// the zero value.
	Presence
)

// Field is one field of a message.
type Field struct {
	Name     string
	Kind     Kind
	Flags    Flag
	Repeated bool
	Values   []string       // Enum
	Msg      func() []Field // Message
	Elem     *Field         // Map
	Doc      string
}

// Has reports whether f carries flag.
func (f Field) Has(flag Flag) bool { return f.Flags&flag != 0 }

// Method is the HTTP binding of one standard method.
type Method struct {
	HTTP     string   // GET, POST, PATCH, DELETE
	Template string   // the google.api.http template
	Query    []string // request fields sent as query parameters
	LRO      bool     // returns an Operation (§3.9)
}

// Resource is one managed Resource type (§8.6).
type Resource struct {
	TypeName      string // "data_database" (the provider prefixes "sylphx_")
	Type          string // "data.sylphx.com/Database"
	Doc           string
	Pattern       string // "orgs/{org}/projects/{project}/envs/{env}/databases/{database}"
	ParentPattern string // "" at the top level
	IDParam       string // "database_id"; "" when the server assigns the id
	BodyField     string // the Create/Update request field carrying the Resource
	SpecRequired  bool
	Reconciled    bool
	Get           *Method
	Create        *Method
	Update        *Method
	Delete        *Method
	Spec          func() []Field
	Status        func() []Field
}
