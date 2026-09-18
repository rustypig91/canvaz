; Included by Tauri before MultiUser.nsh and its install-mode page.
; Elevation must not make an all-users installation the recommended default.
!define MULTIUSER_INSTALLMODE_DEFAULT_CURRENTUSER
!define MULTIUSER_INSTALLMODEPAGE_TEXT_CURRENTUSER "Only me (recommended)"
!define MULTIUSER_INSTALLMODEPAGE_TEXT_ALLUSERS "All users"
!define MULTIUSER_INSTALLMODEPAGE_TEXT_TOP "Choose who can use Canvaz. Only me is recommended, even if you started this installer as administrator. Choose All users only when sharing the installation with other Windows accounts."

!include "${__FILEDIR__}\shortcut-fixes.nsh"
