export type SignInMessages = {
	signIn: string
	enterPassword: string
	createYourAccount: string
	checkInbox: string
	confirmEmail: string
	verifyIdentity: string
	chooseMethod: string
	startWithEmail: string
	checkEmailDescription: string
	confirmEmailDescription: string
	enterEmailCode: (email: string) => string
	enterRecovery: string
	enterAuthenticator: string
	loading: string
	retry: string
	openingProvider: (provider: string) => string
	continueProvider: (provider: string) => string
	orEmail: string
	email: string
	sendingCode: string
	emailCodeAction: string
	continueEmail: string
	waitingPasskey: string
	usePasskey: string
	newHere: string
	createAnAccount: string
	noMethods: string
	password: string
	hidePassword: string
	showPassword: string
	hide: string
	show: string
	minimumPassword: string
	working: string
	createAccount: string
	confirmAndSignIn: string
	changeEmailOrMethod: string
	backToSignIn: string
	emailCode: string
	checkingCode: string
	verifyCode: string
	/** The duration already includes its localized unit. */
	resendIn: (duration: string) => string
	resendCode: string
	changeEmail: string
	usePassword: string
	recoveryCode: string
	authenticatorCode: string
	verifyAndSignIn: string
	useAuthenticator: string
	useRecovery: string
	language: string
	errors: {
		signin: string
		cancelled: string
		use_existing: string
		invalid_credentials: string
		verify_email: string
		acceptance_required: string
		verification_failed: string
		invalid_code: string
		method_unavailable: string
		signup_failed: string
		invalid_input: string
		try_later: string
		unavailable: string
	}
}

export const catalogs = {
	en: {
		signIn: 'Sign in',
		enterPassword: 'Enter your password',
		createYourAccount: 'Create your account',
		checkInbox: 'Check your inbox',
		confirmEmail: 'Confirm your email',
		verifyIdentity: 'Verify it’s you',
		chooseMethod: 'Choose how you’d like to sign in.',
		startWithEmail: 'Use your email to get started.',
		checkEmailDescription:
			'If this address is new, we’ll send a confirmation link. Open it and enter the password you just chose.',
		confirmEmailDescription: 'Enter the email and password you used to sign up.',
		enterEmailCode: (email) => `Enter the six-digit code sent to ${email}.`,
		enterRecovery: 'Enter one of your saved recovery codes.',
		enterAuthenticator: 'Enter the code from your authenticator app.',
		loading: 'Loading sign-in options…',
		retry: 'Try again',
		openingProvider: (provider) => `Opening ${provider}…`,
		continueProvider: (provider) => `Continue with ${provider}`,
		orEmail: 'or use email',
		email: 'Email address',
		sendingCode: 'Sending code…',
		emailCodeAction: 'Email me a code',
		continueEmail: 'Continue with email',
		waitingPasskey: 'Waiting for your passkey…',
		usePasskey: 'Use a passkey',
		newHere: 'New here?',
		createAnAccount: 'Create an account',
		noMethods: 'No sign-in methods are available right now.',
		password: 'Password',
		hidePassword: 'Hide password',
		showPassword: 'Show password',
		hide: 'Hide',
		show: 'Show',
		minimumPassword: 'Use at least 12 characters.',
		working: 'Working…',
		createAccount: 'Create account',
		confirmAndSignIn: 'Confirm and sign in',
		changeEmailOrMethod: 'Change email or sign-in method',
		backToSignIn: 'Back to sign in',
		emailCode: 'Email code',
		checkingCode: 'Checking code…',
		verifyCode: 'Verify code',
		resendIn: (duration) => `Resend in ${duration}`,
		resendCode: 'Resend code',
		changeEmail: 'Change email',
		usePassword: 'Use password instead',
		recoveryCode: 'Recovery code',
		authenticatorCode: 'Authenticator code',
		verifyAndSignIn: 'Verify and sign in',
		useAuthenticator: 'Use authenticator app',
		useRecovery: 'Use a recovery code',
		language: 'Language',
		errors: {
			signin: 'Sign-in didn’t finish. Try again or choose another way to sign in.',
			cancelled: 'Sign-in was canceled. You can try again whenever you’re ready.',
			use_existing:
				'Try the method you originally used to sign in. This provider couldn’t confirm your email.',
			invalid_credentials: 'That email and password didn’t match. Check both and try again.',
			acceptance_required: 'Accept the current terms to continue.',
			verify_email: 'Confirm your email from the link in your inbox before signing in.',
			verification_failed:
				'This confirmation link couldn’t be used. Check that you opened the latest link.',
			invalid_code: 'That code didn’t work. Check it and try again.',
			method_unavailable: 'That sign-in method isn’t available right now. Try another one.',
			signup_failed: 'We couldn’t start your account. Check your details and try again.',
			invalid_input: 'Check your details and try again.',
			try_later: 'Too many attempts. Wait a moment before trying again.',
			unavailable: 'We couldn’t connect. Try again in a moment.',
		},
	},
	'zh-Hant-HK': {
		signIn: '登入',
		enterPassword: '輸入密碼',
		createYourAccount: '建立帳戶',
		checkInbox: '查閱收件箱',
		confirmEmail: '確認電郵地址',
		verifyIdentity: '驗證身分',
		chooseMethod: '請選擇登入方式。',
		startWithEmail: '請輸入電郵地址以建立帳戶。',
		checkEmailDescription:
			'如這個電郵地址尚未登記，我們會發出確認電郵。請開啟電郵內的連結，再輸入剛設定的密碼。',
		confirmEmailDescription: '請輸入登記時使用的電郵地址及密碼。',
		enterEmailCode: (email) => `請輸入發送至 ${email} 的六位數字驗證碼。`,
		enterRecovery: '請輸入其中一個已儲存的復原碼。',
		enterAuthenticator: '請輸入驗證器應用程式顯示的驗證碼。',
		loading: '正在載入登入選項…',
		retry: '再試一次',
		openingProvider: (provider) => `正在開啟 ${provider}…`,
		continueProvider: (provider) => `使用 ${provider} 登入`,
		orEmail: '或使用電郵',
		email: '電郵地址',
		sendingCode: '正在發送驗證碼…',
		emailCodeAction: '發送電郵驗證碼',
		continueEmail: '使用電郵繼續',
		waitingPasskey: '請在裝置上使用通行密鑰…',
		usePasskey: '使用通行密鑰',
		newHere: '未有帳戶？',
		createAnAccount: '建立帳戶',
		noMethods: '目前沒有可用的登入方式。',
		password: '密碼',
		hidePassword: '隱藏密碼',
		showPassword: '顯示密碼',
		hide: '隱藏',
		show: '顯示',
		minimumPassword: '密碼須至少有 12 個字元。',
		working: '處理中…',
		createAccount: '建立帳戶',
		confirmAndSignIn: '確認並登入',
		changeEmailOrMethod: '更改電郵地址或登入方式',
		backToSignIn: '返回登入',
		emailCode: '電郵驗證碼',
		checkingCode: '正在核對驗證碼…',
		verifyCode: '驗證',
		resendIn: (duration) => `${duration}後可重發`,
		resendCode: '重發驗證碼',
		changeEmail: '更改電郵地址',
		usePassword: '改用密碼',
		recoveryCode: '復原碼',
		authenticatorCode: '驗證器驗證碼',
		verifyAndSignIn: '驗證並登入',
		useAuthenticator: '使用驗證器',
		useRecovery: '使用復原碼',
		language: '語言',
		errors: {
			signin: '未能完成登入。請再試一次，或選擇其他登入方式。',
			cancelled: '登入已取消。你可以隨時再試。',
			use_existing: '這項登入服務未能確認你的電郵地址。請改用原來的登入方式。',
			invalid_credentials: '電郵地址與密碼不符。請核對後再試。',
			acceptance_required: '請同意現行條款後繼續。',
			verify_email: '請先開啟確認電郵內的連結，確認電郵地址後再登入。',
			verification_failed: '此連結無法使用。請開啟最新一封確認電郵內的連結。',
			invalid_code: '驗證碼無法使用。請核對後再試。',
			method_unavailable: '暫時無法使用此登入方式。請選擇其他方式。',
			signup_failed: '未能建立帳戶。請核對資料後再試。',
			invalid_input: '請核對資料後再試。',
			try_later: '嘗試次數過多。請稍候再試。',
			unavailable: '未能連線。請稍後再試。',
		},
	},
	'zh-Hant-TW': {
		signIn: '登入',
		enterPassword: '輸入密碼',
		createYourAccount: '建立帳號',
		checkInbox: '查看收件匣',
		confirmEmail: '確認電子郵件地址',
		verifyIdentity: '驗證身分',
		chooseMethod: '請選擇登入方式。',
		startWithEmail: '請先輸入電子郵件地址。',
		checkEmailDescription:
			'如果這個電子郵件地址還沒註冊，我們會寄出確認信。請開啟信中的連結，再輸入剛設定的密碼。',
		confirmEmailDescription: '請輸入註冊時使用的電子郵件地址和密碼。',
		enterEmailCode: (email) => `請輸入寄到 ${email} 的 6 位數驗證碼。`,
		enterRecovery: '請輸入一組已儲存的復原碼。',
		enterAuthenticator: '請輸入驗證器應用程式顯示的驗證碼。',
		loading: '正在載入登入選項…',
		retry: '再試一次',
		openingProvider: (provider) => `正在開啟 ${provider}…`,
		continueProvider: (provider) => `使用 ${provider} 登入`,
		orEmail: '或使用電子郵件',
		email: '電子郵件地址',
		sendingCode: '正在寄送驗證碼…',
		emailCodeAction: '寄送驗證碼',
		continueEmail: '以電子郵件繼續',
		waitingPasskey: '請在裝置上使用通行金鑰…',
		usePasskey: '使用通行金鑰',
		newHere: '還沒有帳號？',
		createAnAccount: '建立帳號',
		noMethods: '目前沒有可用的登入方式。',
		password: '密碼',
		hidePassword: '隱藏密碼',
		showPassword: '顯示密碼',
		hide: '隱藏',
		show: '顯示',
		minimumPassword: '密碼至少需要 12 個字元。',
		working: '處理中…',
		createAccount: '建立帳號',
		confirmAndSignIn: '確認並登入',
		changeEmailOrMethod: '變更電子郵件或登入方式',
		backToSignIn: '返回登入頁',
		emailCode: '電子郵件驗證碼',
		checkingCode: '正在檢查驗證碼…',
		verifyCode: '驗證',
		resendIn: (duration) => `${duration}後可重寄`,
		resendCode: '重寄驗證碼',
		changeEmail: '變更電子郵件',
		usePassword: '改用密碼',
		recoveryCode: '復原碼',
		authenticatorCode: '驗證器驗證碼',
		verifyAndSignIn: '驗證並登入',
		useAuthenticator: '使用驗證器',
		useRecovery: '使用復原碼',
		language: '語言',
		errors: {
			signin: '無法完成登入。請再試一次，或改用其他方式登入。',
			cancelled: '登入已取消。你可以隨時再試。',
			use_existing: '這項登入服務無法確認你的電子郵件地址。請改用原本的登入方式。',
			invalid_credentials: '電子郵件地址和密碼不符。請檢查後再試一次。',
			acceptance_required: '請同意目前的條款後繼續。',
			verify_email: '請先開啟確認信中的連結，確認電子郵件地址後再登入。',
			verification_failed: '這個確認連結無法使用。請確認你開啟的是最新一封確認信中的連結。',
			invalid_code: '這組驗證碼無法使用。請檢查後再試一次。',
			method_unavailable: '這個登入方式目前無法使用。請改用其他方式。',
			signup_failed: '無法建立帳號。請檢查輸入的資料後再試一次。',
			invalid_input: '請檢查輸入的資料後再試一次。',
			try_later: '嘗試次數過多。請稍後再試。',
			unavailable: '目前無法連線。請稍後再試。',
		},
	},
	'zh-Hans': {
		signIn: '登录',
		enterPassword: '输入密码',
		createYourAccount: '创建账号',
		checkInbox: '查看收件箱',
		confirmEmail: '确认邮箱地址',
		verifyIdentity: '验证身份',
		chooseMethod: '请选择登录方式。',
		startWithEmail: '请先输入邮箱地址。',
		checkEmailDescription:
			'如果此邮箱地址尚未注册，我们会发送一封确认邮件。请打开邮件中的链接，再输入刚设置的密码。',
		confirmEmailDescription: '请输入注册时使用的邮箱地址和密码。',
		enterEmailCode: (email) => `请输入发送到 ${email} 的 6 位数字验证码。`,
		enterRecovery: '请输入一个已保存的恢复代码。',
		enterAuthenticator: '请输入验证器应用中的验证码。',
		loading: '正在加载登录选项…',
		retry: '重试',
		openingProvider: (provider) => `正在打开 ${provider}…`,
		continueProvider: (provider) => `使用 ${provider} 登录`,
		orEmail: '或使用邮箱',
		email: '邮箱地址',
		sendingCode: '正在发送验证码…',
		emailCodeAction: '发送邮箱验证码',
		continueEmail: '使用邮箱继续',
		waitingPasskey: '请在设备上使用通行密钥…',
		usePasskey: '使用通行密钥',
		newHere: '还没有账号？',
		createAnAccount: '创建账号',
		noMethods: '当前没有可用的登录方式。',
		password: '密码',
		hidePassword: '隐藏密码',
		showPassword: '显示密码',
		hide: '隐藏',
		show: '显示',
		minimumPassword: '密码至少需要 12 个字符。',
		working: '处理中…',
		createAccount: '创建账号',
		confirmAndSignIn: '确认并登录',
		changeEmailOrMethod: '更改邮箱或登录方式',
		backToSignIn: '返回登录',
		emailCode: '邮箱验证码',
		checkingCode: '正在核对验证码…',
		verifyCode: '验证',
		resendIn: (duration) => `${duration}后可重新发送`,
		resendCode: '重新发送验证码',
		changeEmail: '更改邮箱',
		usePassword: '改用密码',
		recoveryCode: '恢复代码',
		authenticatorCode: '验证器验证码',
		verifyAndSignIn: '验证并登录',
		useAuthenticator: '使用验证器',
		useRecovery: '使用恢复代码',
		language: '语言',
		errors: {
			signin: '未能完成登录。请重试，或选择其他登录方式。',
			cancelled: '登录已取消。你可以随时重试。',
			use_existing: '此登录服务无法确认你的邮箱地址。请改用原来的登录方式。',
			invalid_credentials: '邮箱地址和密码不匹配。请检查后重试。',
			acceptance_required: '请同意当前条款后继续。',
			verify_email: '请先打开确认邮件中的链接，确认邮箱地址后再登录。',
			verification_failed: '此链接无法使用。请打开最新一封确认邮件中的链接。',
			invalid_code: '此验证码无法使用。请检查后重试。',
			method_unavailable: '此登录方式暂不可用，请改用其他方式。',
			signup_failed: '无法创建账号。请检查填写的信息后重试。',
			invalid_input: '请检查填写的信息后重试。',
			try_later: '尝试次数过多。请稍后重试。',
			unavailable: '暂时无法连接，请稍后再试。',
		},
	},
	ja: {
		signIn: 'ログイン',
		enterPassword: 'パスワードを入力',
		createYourAccount: 'アカウントを作成',
		checkInbox: '受信トレイを確認',
		confirmEmail: 'メールアドレスを確認',
		verifyIdentity: '本人確認',
		chooseMethod: 'ログイン方法を選んでください。',
		startWithEmail: 'まず、メールアドレスを入力してください。',
		checkEmailDescription:
			'このメールアドレスが未登録の場合は、確認メールを送信します。メール内のリンクを開き、設定したパスワードを入力してください。',
		confirmEmailDescription: '登録時のメールアドレスとパスワードを入力してください。',
		enterEmailCode: (email) => `${email} に送信した6桁のコードを入力してください。`,
		enterRecovery: '保存してあるリカバリーコードを1つ入力してください。',
		enterAuthenticator: '認証アプリに表示されたコードを入力してください。',
		loading: 'ログイン方法を読み込み中…',
		retry: 'もう一度試す',
		openingProvider: (provider) => `${provider} を開いています…`,
		continueProvider: (provider) => `${provider} でログイン`,
		orEmail: 'またはメールアドレスを使用',
		email: 'メールアドレス',
		sendingCode: 'コードを送信中…',
		emailCodeAction: 'コードをメールで受け取る',
		continueEmail: 'メールアドレスで続ける',
		waitingPasskey: '端末でパスキーを使ってください…',
		usePasskey: 'パスキーを使う',
		newHere: 'アカウントをお持ちでない方',
		createAnAccount: '新規登録',
		noMethods: '現在利用できるログイン方法はありません。',
		password: 'パスワード',
		hidePassword: 'パスワードを非表示にする',
		showPassword: 'パスワードを表示',
		hide: '非表示',
		show: '表示',
		minimumPassword: 'パスワードは12文字以上にしてください。',
		working: '処理中…',
		createAccount: 'アカウントを作成',
		confirmAndSignIn: '確認してログイン',
		changeEmailOrMethod: 'メールアドレスやログイン方法を変更',
		backToSignIn: 'ログインに戻る',
		emailCode: 'メールの確認コード',
		checkingCode: 'コードを確認中…',
		verifyCode: 'コードを確認',
		resendIn: (duration) => `再送信まであと${duration}`,
		resendCode: 'コードを再送信',
		changeEmail: 'メールアドレスを変更',
		usePassword: 'パスワードを使う',
		recoveryCode: 'リカバリーコード',
		authenticatorCode: '認証アプリのコード',
		verifyAndSignIn: '認証してログイン',
		useAuthenticator: '認証アプリを使う',
		useRecovery: 'リカバリーコードを使う',
		language: '言語',
		errors: {
			signin: 'ログインを完了できませんでした。もう一度試すか、別のログイン方法をお選びください。',
			cancelled: 'ログインがキャンセルされました。いつでもやり直せます。',
			use_existing:
				'このサービスではメールアドレスを確認できませんでした。最初に使った方法でログインしてください。',
			invalid_credentials:
				'メールアドレスとパスワードが一致しません。両方を確認して、もう一度お試しください。',
			acceptance_required: '続行するには、現在の利用規約に同意してください。',
			verify_email:
				'確認メール内のリンクを開き、メールアドレスを確認してからログインしてください。',
			verification_failed: 'この確認リンクは使えません。最新の確認メールのリンクをお試しください。',
			invalid_code: 'このコードは使えません。入力内容を確認して、もう一度お試しください。',
			method_unavailable: '現在この方法ではログインできません。別のログイン方法をお選びください。',
			signup_failed:
				'アカウントを作成できませんでした。入力内容を確認して、もう一度お試しください。',
			invalid_input: '入力内容を確認して、もう一度お試しください。',
			try_later: '試行回数が上限に達しました。しばらく待ってからお試しください。',
			unavailable: '接続できませんでした。しばらく待ってからお試しください。',
		},
	},
	ko: {
		signIn: '로그인',
		enterPassword: '비밀번호를 입력하세요',
		createYourAccount: '계정 만들기',
		checkInbox: '이메일을 확인하세요',
		confirmEmail: '이메일 주소 확인',
		verifyIdentity: '본인 확인',
		chooseMethod: '로그인 방법을 선택하세요.',
		startWithEmail: '이메일 주소로 시작하세요.',
		checkEmailDescription:
			'처음 사용하는 이메일 주소라면 확인 링크를 보내 드립니다. 링크를 열고 방금 설정한 비밀번호를 입력하세요.',
		confirmEmailDescription: '가입할 때 사용한 이메일 주소와 비밀번호를 입력하세요.',
		enterEmailCode: (email) => `${email} 주소로 보낸 6자리 인증 코드를 입력하세요.`,
		enterRecovery: '저장해 둔 복구 코드 중 하나를 입력하세요.',
		enterAuthenticator: '인증 앱에 표시된 코드를 입력하세요.',
		loading: '로그인 방법을 불러오는 중…',
		retry: '다시 시도',
		openingProvider: (provider) => `${provider} 로그인 화면을 여는 중…`,
		continueProvider: (provider) => `${provider} 계정으로 계속`,
		orEmail: '또는 이메일로 계속',
		email: '이메일 주소',
		sendingCode: '인증 코드를 보내는 중…',
		emailCodeAction: '이메일로 인증 코드 받기',
		continueEmail: '이메일로 계속',
		waitingPasskey: '패스키 인증을 기다리는 중…',
		usePasskey: '패스키 사용',
		newHere: '계정이 없으신가요?',
		createAnAccount: '계정 만들기',
		noMethods: '지금은 사용할 수 있는 로그인 방법이 없습니다.',
		password: '비밀번호',
		hidePassword: '비밀번호 숨기기',
		showPassword: '비밀번호 보기',
		hide: '숨기기',
		show: '보기',
		minimumPassword: '12자 이상 입력하세요.',
		working: '처리 중…',
		createAccount: '계정 만들기',
		confirmAndSignIn: '확인하고 로그인',
		changeEmailOrMethod: '이메일 주소 또는 로그인 방법 변경',
		backToSignIn: '로그인으로 돌아가기',
		emailCode: '이메일 인증 코드',
		checkingCode: '인증 코드를 확인하는 중…',
		verifyCode: '인증 코드 확인',
		resendIn: (duration) => `${duration} 후 다시 보낼 수 있습니다.`,
		resendCode: '인증 코드 다시 보내기',
		changeEmail: '이메일 주소 변경',
		usePassword: '비밀번호로 로그인',
		recoveryCode: '복구 코드',
		authenticatorCode: '인증 앱 코드',
		verifyAndSignIn: '인증하고 로그인',
		useAuthenticator: '인증 앱 사용',
		useRecovery: '복구 코드 사용',
		language: '언어',
		errors: {
			signin: '로그인하지 못했습니다. 다시 시도하거나 다른 로그인 방법을 선택하세요.',
			cancelled: '로그인이 취소되었습니다. 언제든지 다시 시도할 수 있습니다.',
			use_existing:
				'이 로그인 서비스에서 이메일 주소를 확인할 수 없습니다. 처음 사용한 방법으로 로그인하세요.',
			invalid_credentials:
				'이메일 주소 또는 비밀번호가 올바르지 않습니다. 확인한 뒤 다시 시도하세요.',
			acceptance_required: '계속하려면 현재 약관에 동의하세요.',
			verify_email: '로그인하기 전에 받은 이메일의 링크를 열어 이메일 주소를 확인하세요.',
			verification_failed:
				'이 확인 링크를 사용할 수 없습니다. 가장 최근에 받은 링크를 열어 보세요.',
			invalid_code: '인증 코드가 올바르지 않습니다. 코드를 확인한 뒤 다시 시도하세요.',
			method_unavailable: '지금은 이 방법으로 로그인할 수 없습니다. 다른 방법을 선택하세요.',
			signup_failed: '계정을 만들지 못했습니다. 입력 내용을 확인한 뒤 다시 시도하세요.',
			invalid_input: '입력 내용을 확인한 뒤 다시 시도하세요.',
			try_later: '시도 횟수가 너무 많습니다. 잠시 후 다시 시도하세요.',
			unavailable: '연결하지 못했습니다. 잠시 후 다시 시도하세요.',
		},
	},
} satisfies Record<string, SignInMessages>

export type AuthLocale = keyof typeof catalogs

export const localeNames = {
	en: 'English',
	'zh-Hant-HK': '繁體中文（香港）',
	'zh-Hant-TW': '繁體中文（台灣）',
	'zh-Hans': '简体中文',
	ja: '日本語',
	ko: '한국어',
} satisfies Record<AuthLocale, string>
